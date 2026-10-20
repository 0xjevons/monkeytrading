mod api;
mod config;
mod custody;
mod feeds;
mod log;
mod sequencer;
mod tee;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};
use engine_core::command::Command;
use engine_core::types::AccountId;
use tokio::sync::{broadcast, mpsc};
use tracing_subscriber::EnvFilter;

use crate::config::{AuthMode, CustodyProvider, FeedProvider, NodeConfig, TeeProvider};
use crate::sequencer::{Request, Sequencer, SequencerConfig};
use crate::tee::{Attestor, ConfigfsTsm, EnclaveKey, MockAttestor};

#[derive(Parser)]
#[command(about = "House-counterparty CFD engine, built to run inside a TEE")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the node.
    Run {
        #[arg(long, default_value = "config/dev.toml")]
        config: PathBuf,
    },
    /// Replay a published log and verify every checkpoint root.
    Replay {
        #[arg(long)]
        log: PathBuf,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();
    match Cli::parse().cmd {
        Cmd::Run { config } => run(NodeConfig::load(&config)?).await,
        Cmd::Replay { log } => {
            let r = sequencer::replay(&log::read_all(&log)?)?;
            println!(
                "verified: {} commands, {} checkpoints, final root 0x{}",
                r.commands,
                r.checkpoints,
                hex::encode(r.engine.state_root())
            );
            Ok(())
        }
    }
}

async fn run(cfg: NodeConfig) -> anyhow::Result<()> {
    if cfg.operator.rotation_enabled {
        bail!("operator rotation is not implemented yet; set operator.rotation_enabled = false");
    }
    if cfg.node.auth == AuthMode::None {
        tracing::warn!("API authentication is OFF: development only");
    }
    custody::check(&cfg.custody)?;

    let attestor: Box<dyn Attestor> = match cfg.tee.provider {
        TeeProvider::Mock => {
            tracing::warn!("mock TEE: attestation reports are not backed by hardware");
            Box::new(MockAttestor)
        }
        TeeProvider::ConfigfsTsm => Box::new(ConfigfsTsm::default()),
    };
    let key = Arc::new(EnclaveKey::generate());
    let attestation = Arc::new(tee::attest_key(attestor.as_ref(), &key).context("attesting enclave key")?);
    tracing::info!(enclave = %key.address_hex(), provider = %attestation.provider, "enclave key attested");

    let vault: AccountId = cfg.custody.vault.parse().context("custody.vault")?;
    let (stream_tx, _) = broadcast::channel(65_536);
    let (withdrawals_tx, withdrawals_rx) = mpsc::unbounded_channel();
    let mut seq = Sequencer::recover(
        SequencerConfig {
            log_path: &cfg.node.log_path,
            checkpoint_every: cfg.node.checkpoint_every,
            chain_id: cfg.custody.chain_id,
            vault,
        },
        key.clone(),
        stream_tx.clone(),
        withdrawals_tx,
    )?;
    if seq.is_fresh() {
        tracing::info!(markets = cfg.genesis.markets.len(), "sequencing genesis");
        seq.genesis(&cfg.genesis)?;
    }

    let (tx, rx) = mpsc::channel::<Request>(65_536);
    // TODO: pin the sequencer to a dedicated core once latency matters.
    let mut sequencer = tokio::spawn(seq.run(rx));

    let ticks = tx.clone();
    let tick_ms = cfg.node.tick_ms;
    tokio::spawn(async move {
        let mut every = tokio::time::interval(Duration::from_millis(tick_ms));
        loop {
            every.tick().await;
            if ticks.send(Request::Submit { cmd: Command::Tick, reply: None }).await.is_err() {
                return;
            }
        }
    });

    let feed_tx = tx.clone();
    let feeds = cfg.feeds;
    tokio::spawn(async move {
        match feeds.provider {
            FeedProvider::Mock => feeds::run_mock(&feeds, feed_tx).await,
        }
    });

    let custody_cfg = cfg.custody;
    let dev_deposits = custody_cfg.provider == CustodyProvider::Mock;
    tokio::spawn(async move {
        if let Err(e) = custody::run(&custody_cfg, withdrawals_rx).await {
            tracing::error!("custody stopped: {e:#}");
        }
    });

    let app = api::router(api::ApiState {
        tx,
        stream: stream_tx,
        attestation,
        auth: cfg.node.auth,
        dev_deposits,
    });
    let listener = tokio::net::TcpListener::bind(cfg.node.listen).await?;
    tracing::info!(listen = %cfg.node.listen, "api up");
    let mut server = tokio::spawn(async move { axum::serve(listener, app).await });

    tokio::select! {
        r = &mut sequencer => {
            r.context("sequencer task panicked")??;
            bail!("sequencer stopped");
        }
        r = &mut server => {
            r.context("api task panicked")??;
            bail!("api server stopped");
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("shutting down");
            Ok(())
        }
    }
}
