//! Node wiring: attest the enclave key, recover the engine from its log, then run the sequencer,
//! ticker, price feeds, custody, and API as supervised tasks. Any task stopping stops the node.
//! On SIGTERM or ctrl-c, every task but the sequencer is stopped; the sequencer then drains what
//! is queued and writes a final checkpoint.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use engine_core::command::Command;
use engine_core::types::AccountId;
use tokio::net::TcpListener;
use tokio::signal::ctrl_c;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{Semaphore, broadcast, mpsc, watch};
use tokio::task::{JoinError, JoinSet};
use tokio::time::{interval, timeout};
use tracing::{error, info, warn};
use tracing_subscriber::{EnvFilter, fmt};

use crate::api::{ApiState, MAX_BACKFILLS, router};
use crate::clock::now_ms;
use crate::config::{AuthMode, CustodyProvider, FeedProvider, NodeConfig, TeeProvider};
use crate::feeds::venue::{self, Timing};
use crate::log::Manifest;
use crate::sequencer::{Request, Sequencer, SequencerConfig};
use crate::tee::{Attestor, ConfigfsTsm, EnclaveKey, MockAttestor, attest_key};
use crate::{custody, feeds};

/// How long the sequencer gets to drain and checkpoint after a shutdown signal.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);
const CHANNEL_CAPACITY: usize = 65_536;

type TaskResult = (&'static str, anyhow::Result<()>);

/// Installs the global log subscriber at `level`, a `tracing` filter directive.
/// Error cases:
/// - `level` is not a valid filter directive
pub fn init_logging(level: &str) -> anyhow::Result<()> {
    let filter = EnvFilter::try_new(level).context("log level")?;
    fmt().with_env_filter(filter).init();
    Ok(())
}

/// Runs the node until a shutdown signal, or until the first supervised task stops.
/// Error cases:
/// - operator rotation is enabled, or the custody provider is not implemented
/// - `feeds.provider = "venues"` without `[feeds.venues]`, or the TLS config cannot be built
/// - attesting the enclave key, recovering the log, activating the protocol (including a log that
///   records another client auth), sequencing genesis, or binding the listener fails
/// - any supervised task stops or panics, or the sequencer cannot drain in time
pub async fn run(cfg: NodeConfig) -> anyhow::Result<()> {
    if cfg.operator.rotation_enabled {
        bail!("operator rotation is not implemented yet; set operator.rotation_enabled = false");
    }
    if cfg.node.auth == AuthMode::None {
        warn!("API authentication is OFF: development only");
    }
    custody::check(&cfg.custody)?;

    let attestor: Box<dyn Attestor> = match cfg.tee.provider {
        TeeProvider::Mock => {
            warn!("mock TEE: attestation reports are not backed by hardware");
            Box::new(MockAttestor)
        }
        TeeProvider::ConfigfsTsm => Box::new(ConfigfsTsm::default()),
    };
    let key = Arc::new(EnclaveKey::generate());
    let attestation = Arc::new(attest_key(attestor.as_ref(), &key).context("attesting enclave key")?);
    info!(enclave = %key.address_hex(), provider = %attestation.provider, "enclave key attested");

    let vault: AccountId = cfg.custody.vault.parse().context("custody.vault")?;
    let client_auth = cfg.client_auth()?;
    let (stream_tx, _) = broadcast::channel(CHANNEL_CAPACITY);
    let (withdrawals_tx, withdrawals_rx) = mpsc::unbounded_channel();
    let (manifest_tx, manifest_rx) = watch::channel(Manifest::default());
    let seq_cfg = SequencerConfig {
        log_dir: &cfg.node.log_dir,
        segment_commands: cfg.node.segment_commands,
        manifest: manifest_tx,
        checkpoint_every: cfg.node.checkpoint_every,
        chain_id: cfg.custody.chain_id,
        vault,
        clock: now_ms,
        client_auth: client_auth.clone(),
    };
    let mut seq = Sequencer::recover(seq_cfg, key.clone(), stream_tx.clone(), withdrawals_tx)
        .inspect_err(|e| error!(alert = "log_recovery_failed", "refusing to serve: {e:#}"))?;
    seq.activate_protocol()
        .inspect_err(|e| error!(alert = "protocol_activation_failed", "refusing to serve: {e:#}"))?;
    if seq.needs_genesis(&cfg.genesis) {
        info!(markets = cfg.genesis.markets.len(), "sequencing genesis");
        seq.genesis(&cfg.genesis)?;
    }

    let (tx, rx) = mpsc::channel::<Request>(CHANNEL_CAPACITY);
    // TODO: pin the sequencer to a dedicated core once latency matters.
    let mut sequencer = tokio::spawn(seq.run(rx));
    let mut tasks: JoinSet<TaskResult> = JoinSet::new();

    let tick = cfg.node.tick;
    let ticks = tx.clone();
    tasks.spawn(async move { ("ticker", run_ticker(ticks, tick).await) });

    let feeds = cfg.feeds;
    match feeds.provider {
        FeedProvider::Mock => {
            let feed_tx = tx.clone();
            tasks.spawn(async move {
                feeds::run_mock(&feeds, feed_tx).await;
                ("feeds", Ok(()))
            });
        }
        FeedProvider::Venues => {
            let venues = feeds.venues.context("feeds.provider = \"venues\" needs [feeds.venues]")?;
            let timing = Timing::new(feeds.interval, &venues);
            let tls = venue::tls_config()?;
            for conn in venues.connections {
                let (name, tls, feed_tx) = (conn.venue.name(), tls.clone(), tx.clone());
                info!(venue = name, url = %conn.url, symbols = conn.symbols.len(), "starting venue feed");
                tasks.spawn(async move { (name, venue::run(conn, timing, tls, feed_tx).await) });
            }
        }
    }

    let custody_cfg = cfg.custody;
    let dev_deposits = custody_cfg.provider == CustodyProvider::Mock;
    tasks.spawn(async move { ("custody", custody::run(&custody_cfg, withdrawals_rx).await) });

    let listener = TcpListener::bind(cfg.node.listen).await.with_context(|| format!("binding {}", cfg.node.listen))?;
    let app = router(ApiState {
        tx,
        stream: stream_tx,
        attestation,
        auth: client_auth,
        dev_deposits,
        clock: now_ms,
        log_dir: cfg.node.log_dir.clone(),
        manifest: manifest_rx,
        backfills: Arc::new(Semaphore::new(MAX_BACKFILLS)),
    });
    info!(listen = %cfg.node.listen, "api up");
    tasks.spawn(async move { ("api", axum::serve(listener, app).await.context("api server")) });

    let outcome = tokio::select! {
        joined = tasks.join_next() => Err(stopped_task(joined)),
        joined = &mut sequencer => {
            tasks.shutdown().await;
            sequencer_outcome(joined)?;
            bail!("sequencer stopped");
        }
        signal = shutdown_signal() => signal.map(|()| info!("shutting down")),
    };
    // Stopping the other tasks drops every sender, so the sequencer drains and checkpoints.
    tasks.shutdown().await;
    let drained = timeout(SHUTDOWN_GRACE, sequencer).await.context("sequencer did not drain in time")?;
    sequencer_outcome(drained)?;
    outcome
}

/// Sequences a `Tick` every `every`, so sessions and rollovers fire without other traffic.
/// Error cases:
/// - the sequencer has gone away
async fn run_ticker(tx: mpsc::Sender<Request>, every: Duration) -> anyhow::Result<()> {
    let mut ticks = interval(every);
    loop {
        ticks.tick().await;
        tx.send(Request::Submit { cmd: Command::Tick, reply: None }).await.map_err(|_| anyhow!("sequencer gone"))?;
    }
}

/// Resolves on SIGTERM or ctrl-c.
/// Error cases:
/// - a signal handler cannot be installed
async fn shutdown_signal() -> anyhow::Result<()> {
    let mut term = signal(SignalKind::terminate()).context("installing SIGTERM handler")?;
    tokio::select! {
        r = ctrl_c() => r.context("listening for ctrl-c"),
        _ = term.recv() => Ok(()),
    }
}

/// The error for a supervised task that stopped, raised as a critical alert.
fn stopped_task(joined: Option<Result<TaskResult, JoinError>>) -> anyhow::Error {
    let err = match joined {
        Some(Ok((name, Ok(())))) => anyhow!("{name} task stopped"),
        Some(Ok((name, Err(e)))) => e.context(format!("{name} task failed")),
        Some(Err(e)) => anyhow!(e).context("task panicked"),
        None => anyhow!("no tasks running"),
    };
    error!(alert = "task_stopped", "{err:#}");
    err
}

/// The sequencer's own result, or its panic.
/// Error cases:
/// - the sequencer panicked, or returned an error (a log write failure)
fn sequencer_outcome(joined: Result<anyhow::Result<()>, JoinError>) -> anyhow::Result<()> {
    joined.context("sequencer task panicked")?.inspect_err(|e| error!(alert = "sequencer_failed", "{e:#}"))
}
