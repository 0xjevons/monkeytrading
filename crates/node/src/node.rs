//! Node wiring: attest the enclave key and the API's TLS key, recover the engine from its log and
//! the history index, then run the sequencer, ticker, price feeds, custody, trusted time check,
//! history index, API, and rate-limit upkeep as supervised tasks. Any task stopping stops the node.
//! On SIGTERM or ctrl-c, every task but the sequencer is stopped; the sequencer then drains what is
//! queued and writes a final checkpoint.

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

use crate::api::{ApiState, Feed, MAX_BACKFILLS, router};
use crate::clock::{Clock, SystemClock, now_ms};
use crate::config::{AuthMode, CustodyProvider, FeedProvider, NodeConfig, TeeProvider};
use crate::feeds::venue::{self, Timing};
use crate::history::History;
use crate::limits::{ApiLimits, run_retain};
use crate::log::{Manifest, SegmentedLog};
use crate::sequencer::{Request, Sequencer, SequencerConfig};
use crate::tee::{Attestor, ConfigfsTsm, EnclaveKey, MockAttestor, attest_key};
use crate::tls::{TlsIdentity, serve};
use crate::{custody, feeds, trusted_time};

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
/// - generating the TLS key, attesting the keys, opening the history index (including one the log
///   does not match), recovering the log (including a log that records another client auth), or
///   binding the listener fails
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
    let tls = TlsIdentity::generate()?;
    let attestation = Arc::new(
        attest_key(attestor.as_ref(), &key, &tls.public_key_sha256).context("attesting the enclave and TLS keys")?,
    );
    info!(
        enclave = %key.address_hex(),
        tls_public_key_sha256 = %attestation.tls_public_key_sha256,
        provider = %attestation.provider,
        "enclave and TLS keys attested"
    );

    // One read of the log: the history index is checked against it before recovery replays it and
    // writes anything, so a refusal leaves the log as it was found, and no other copy of the log can
    // come between the check and the replay.
    let log = SegmentedLog::open(&cfg.node.log_dir)
        .inspect_err(|e| error!(alert = "log_recovery_failed", "refusing to serve: {e:#}"))?;
    let mut history = History::open(&cfg.history, &log.entries)
        .inspect_err(|e| error!(alert = "history_recovery_failed", "refusing to serve: {e:#}"))?;

    let vault: AccountId = cfg.custody.vault.parse().context("custody.vault")?;
    let client_auth = cfg.client_auth()?;
    let (stream_tx, _) = broadcast::channel(CHANNEL_CAPACITY);
    let (withdrawals_tx, withdrawals_rx) = mpsc::unbounded_channel();
    let (manifest_tx, manifest_rx) = watch::channel(Manifest::default());
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let seq_cfg = SequencerConfig {
        log_dir: &cfg.node.log_dir,
        segment_commands: cfg.node.segment_commands,
        manifest: manifest_tx,
        checkpoint_every: cfg.node.checkpoint_every,
        chain_id: cfg.custody.chain_id,
        vault,
        clock: clock.clone(),
        max_drift: cfg.time.max_drift,
        client_auth: client_auth.clone(),
    };
    let mut seq = Sequencer::recover_from(log, seq_cfg, key.clone(), stream_tx.clone(), withdrawals_tx)
        .inspect_err(|e| error!(alert = "log_recovery_failed", "refusing to serve: {e:#}"))?;
    // Protocol activation and genesis wait, like any command, for a stamp trusted time admits;
    // with no sample yet, they are queued.
    seq.activate_protocol()
        .inspect_err(|e| error!(alert = "protocol_activation_failed", "refusing to serve: {e:#}"))?;
    if seq.needs_genesis() {
        info!(markets = cfg.genesis.markets.len(), "genesis queued for the first trusted time sample");
        seq.genesis(&cfg.genesis)?;
    }
    history.catch_up_to(seq.last_seq());

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

    let time_cfg = cfg.time;
    let time_tx = tx.clone();
    tasks.spawn(async move { ("trusted_time", trusted_time::run(time_cfg, clock, time_tx).await) });

    let custody_cfg = cfg.custody;
    let dev_deposits = custody_cfg.provider == CustodyProvider::Mock;
    tasks.spawn(async move { ("custody", custody::run(&custody_cfg, withdrawals_rx).await) });

    let backfills = Arc::new(Semaphore::new(MAX_BACKFILLS));
    let history_reader = history.reader();
    let history_feed = Feed::new(
        stream_tx.subscribe(),
        cfg.node.log_dir.clone(),
        manifest_rx.clone(),
        backfills.clone(),
        Some(history.resume_at()),
    );
    tasks.spawn(async move { ("history", history.run(history_feed).await) });

    let listener = TcpListener::bind(cfg.node.listen).await.with_context(|| format!("binding {}", cfg.node.listen))?;
    let limits = Arc::new(ApiLimits::new(&cfg.api));
    let app = router(ApiState {
        tx,
        stream: stream_tx,
        attestation,
        auth: client_auth,
        dev_deposits,
        clock: now_ms,
        log_dir: cfg.node.log_dir.clone(),
        manifest: manifest_rx,
        backfills,
        limits: limits.clone(),
        history: history_reader,
    });
    info!(listen = %cfg.node.listen, "api up over TLS");
    let (api, retained) = (cfg.api, limits.clone());
    tasks.spawn(async move { ("api", serve(listener, tls.config, app, limits, api).await) });
    tasks.spawn(async move { ("rate limits", run_retain(retained).await) });

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::path::Path;

    use crate::log::MANIFEST;
    use crate::test_support::{TestHistory, TestSequencer, deposit, dev_config, scratch_dir};

    #[tokio::test]
    async fn a_history_index_the_log_contradicts_stops_the_node_before_it_writes_to_the_log() {
        let (dir, index) = (scratch_dir("node-history-refused"), scratch_dir("node-history-refused-index"));
        let node = TestSequencer::start(&dir, 1024);
        let history = TestHistory::start(&node, &dir, &index).await.unwrap();
        node.submit(deposit(1, "dep-1")).await;
        // The log as it stands now, which a host that rolls the log back would restore.
        let older = scratch_dir("node-history-refused-older");
        fs::create_dir_all(&older).unwrap();
        for file in fs::read_dir(&dir).unwrap() {
            let file = file.unwrap();
            fs::copy(file.path(), older.join(file.file_name())).unwrap();
        }
        node.submit(deposit(1, "dep-2")).await;
        node.stop().await;
        history.finish().await.unwrap();
        // A listing a crash cut short, which recovery trims.
        let mut manifest = OpenOptions::new().append(true).open(older.join(MANIFEST)).unwrap();
        manifest.write_all(br#"{"file":"#).unwrap();
        let before = files(&older);

        let mut cfg = dev_config();
        (cfg.node.log_dir, cfg.history.dir) = (older.clone(), index);
        cfg.node.listen = "127.0.0.1:0".parse().unwrap();
        let refused = timeout(Duration::from_secs(10), run(cfg)).await.expect("the node stops in time");
        let err = refused.expect_err("the index took a command the log no longer holds");
        assert!(format!("{err:#}").contains("does not match the log"), "{err:#}");
        assert_eq!(files(&older), before, "the log is as it was found");
    }

    // ── Helpers ──

    /// Every file in `dir` and its bytes.
    fn files(dir: &Path) -> BTreeMap<String, Vec<u8>> {
        let names = fs::read_dir(dir).unwrap().map(|file| file.unwrap().file_name().into_string().unwrap());
        names.map(|name| (name.clone(), fs::read(dir.join(name)).unwrap())).collect()
    }
}
