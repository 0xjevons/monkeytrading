//! Node wiring: attest the enclave key and the API's TLS key, recover the engine from its log and
//! the history index, then run the sequencer, ticker, price feeds, custody, trusted time check,
//! history index, API, rate-limit upkeep, monitor, metrics listener, and alert routing as
//! supervised tasks. Any task stopping stops the node. On SIGTERM or ctrl-c, every task but the
//! sequencer is stopped; the sequencer then drains what is queued and writes a final checkpoint,
//! and the alerts already logged are delivered.

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

use crate::alerts::Routing;
use crate::api::{ApiState, Feed, MAX_BACKFILLS, router};
use crate::clock::{Clock, SystemClock, now_ms};
use crate::config::{AuthMode, CustodyProvider, FeedProvider, NodeConfig, TeeProvider};
use crate::custody::Custody;
use crate::feeds;
use crate::feeds::venue::{self, Timing};
use crate::history::{History, HistoryCheck};
use crate::limits::{ApiLimits, run_retain};
use crate::log::Manifest;
use crate::metrics::Metrics;
use crate::sequencer::{ReplayedLog, Request, Sequencer, SequencerConfig};
use crate::tee::{Attestor, ConfigfsTsm, EnclaveKey, MockAttestor, attest_key};
use crate::telemetry::Telemetry;
use crate::tls::{TlsIdentity, serve};
use crate::{metrics, monitor, trusted_time};

/// How long the sequencer gets to drain and checkpoint after a shutdown signal.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);
const CHANNEL_CAPACITY: usize = 65_536;
type TaskResult = (&'static str, anyhow::Result<()>);

/// Runs the node until a shutdown signal, or until the first supervised task stops, then delivers
/// the alerts already logged, for up to `SHUTDOWN_GRACE`.
/// Error cases:
/// - see `serve_node`
pub async fn run(cfg: NodeConfig, telemetry: Telemetry) -> anyhow::Result<()> {
    let Telemetry { metrics, alerts, pages_critical } = telemetry;
    if !pages_critical {
        warn!("no alerts.critical webhook: Critical alerts are only logged");
    }
    let mut routing = alerts.spawn(cfg.alerts.episode_quiet)?;
    let outcome = serve_node(cfg, metrics, &mut routing).await;
    routing.drain(SHUTDOWN_GRACE).await;
    outcome
}

/// Runs every task but alert routing, which it supervises.
/// Error cases:
/// - operator rotation is enabled
/// - `feeds.provider = "venues"` without `[feeds.venues]`, or the TLS config cannot be built
/// - generating the TLS key, attesting the keys, opening the history index (or a fresh one in place
///   of an index the log does not match), recovering the log (including a log that records another
///   client auth or operator, or whose genesis did not apply in full), connecting custody, or
///   binding a listener fails
/// - the engine would reject a genesis command
/// - any supervised task stops or panics, or the sequencer cannot drain in time
async fn serve_node(cfg: NodeConfig, metrics: Arc<Metrics>, routing: &mut Routing) -> anyhow::Result<()> {
    if cfg.operator.rotation_enabled {
        bail!("operator rotation is not implemented yet; set operator.rotation_enabled = false");
    }
    if cfg.node.auth == AuthMode::None {
        warn!("API authentication is OFF: development only");
    }

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

    let (log, mut history) = read_log_and_history(&cfg)?;

    let vault: AccountId = cfg.custody.vault.parse().context("custody.vault")?;
    let client_auth = cfg.client_auth()?;
    let operator = cfg.operator()?;
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
        operator,
        metrics: metrics.clone(),
        snapshots: cfg.snapshots.as_ref(),
    };
    let mut seq = Sequencer::recover_from(log, seq_cfg, key.clone(), stream_tx.clone(), withdrawals_tx)
        .inspect_err(|e| error!(alert = "log_recovery_failed", "refusing to serve: {e:#}"))?;
    let custody = Custody::connect(&cfg.custody, seq.last_seq())
        .inspect_err(|e| error!(alert = "custody_connect_failed", "refusing to serve: {e:#}"))?;
    // Protocol activation and genesis wait, like any command, for a stamp trusted time admits;
    // with no sample yet, they are queued. Parameters an older log set take the config's toxicity
    // notional weighting first, which the latest version needs.
    let weighting = cfg.genesis.params.toxicity.notional.context("genesis.params.toxicity.notional")?;
    seq.carry_toxicity_notional(weighting)
        .and_then(|()| seq.activate_protocol())
        .inspect_err(|e| error!(alert = "protocol_activation_failed", "refusing to serve: {e:#}"))?;
    if seq.needs_genesis() {
        info!(markets = cfg.genesis.markets.len(), "genesis queued for the first trusted time sample");
        seq.genesis(&cfg.genesis).inspect_err(|e| error!(alert = "genesis_refused", "refusing to serve: {e:#}"))?;
    }
    history.catch_up_to(seq.last_seq());

    let (tx, rx) = mpsc::channel::<Request>(CHANNEL_CAPACITY);
    // TODO: pin the sequencer to a dedicated core once latency matters.
    let mut sequencer = tokio::spawn(seq.run(rx));
    let mut tasks: JoinSet<TaskResult> = JoinSet::new();

    let tick = cfg.node.tick;
    let ticks = tx.clone();
    tasks.spawn(async move { ("ticker", run_ticker(ticks, tick).await) });

    // The monitor exports quote times only for the sources this config still feeds.
    let sources = feeds::configured_sources(&cfg.feeds);
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

    let mock_custody = cfg.custody.provider == CustodyProvider::Mock;
    let custody_tx = tx.clone();
    tasks.spawn(async move { ("custody", custody.run(withdrawals_rx, custody_tx).await) });

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

    let monitor_tx = tx.clone();
    let (monitor_metrics, no_price_after) = (metrics.clone(), cfg.alerts.no_price_after);
    tasks.spawn(async move { ("monitor", monitor::run(monitor_tx, monitor_metrics, no_price_after, sources).await) });

    let metrics_listen = cfg.metrics.listen;
    let metrics_listener =
        TcpListener::bind(metrics_listen).await.with_context(|| format!("binding {metrics_listen}"))?;
    info!(listen = %metrics_listen, "metrics up");
    let metrics_app = metrics::router(metrics);
    tasks.spawn(async move { ("metrics", axum::serve(metrics_listener, metrics_app).await.context("metrics server")) });

    let listener = TcpListener::bind(cfg.node.listen).await.with_context(|| format!("binding {}", cfg.node.listen))?;
    let limits = Arc::new(ApiLimits::new(&cfg.api));
    let app = router(ApiState {
        tx,
        stream: stream_tx,
        attestation,
        auth: client_auth,
        operator,
        mock_custody,
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
        err = routing.stopped() => {
            error!(alert = "task_stopped", "{err:#}");
            Err(err)
        }
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

/// Recovers before any append, checking history against that read during full replay or against
/// its cursor's segment after authenticated snapshot recovery. A mismatched index is rebuilt.
/// Error cases:
/// - the history index cannot be opened, nor a fresh one in its place (see `HistoryCheck`)
/// - the log cannot be read or replayed (see `Sequencer::read_log`)
fn read_log_and_history(cfg: &NodeConfig) -> anyhow::Result<(ReplayedLog, History)> {
    let history_failed = |e: &anyhow::Error| error!(alert = "history_recovery_failed", "refusing to serve: {e:#}");
    let open_history = if cfg.snapshots.is_some() { HistoryCheck::open_cursor } else { HistoryCheck::open };
    let mut check = open_history(&cfg.history).inspect_err(history_failed)?;
    let mut read = || {
        if cfg.snapshots.is_some() {
            let vault = cfg.custody.vault.parse().context("custody.vault")?;
            let log = Sequencer::read_log_with_snapshots(
                &cfg.node.log_dir,
                cfg.snapshots.as_ref(),
                cfg.custody.chain_id,
                vault,
            )?;
            check.check_log(&cfg.node.log_dir, log.last_seq())?;
            Ok(log)
        } else {
            Sequencer::read_log(&cfg.node.log_dir, |entry| check.observe(entry))
        }
    };
    let log = read().inspect_err(|e| error!(alert = "log_recovery_failed", "refusing to serve: {e:#}"))?;
    let history = check.finish().inspect_err(history_failed)?;
    Ok((log, history))
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
    use std::path::{Path, PathBuf};
    use std::{iter, net};

    use crate::alerts::{self, AlertLayer, Routes};
    use crate::config::HistorySection;
    use crate::history::INDEX_FILE;
    use crate::log::{LogEntry, MANIFEST, Offset, segment_seq};
    use crate::test_support::{
        RETENTION, TestHistory, TestSequencer, capture_alerts, deposit, dev_config, log_entries, open_history,
        scratch_dir,
    };

    /// Startup's one read of the log decides the history index's fate: an index that took the log's
    /// last command resumes after it, with no alert and no rebuild.
    #[tokio::test]
    async fn startup_resumes_a_history_index_its_read_of_the_log_matches() {
        let (dir, index) = (scratch_dir("node-history-resumes"), scratch_dir("node-history-resumes-index"));
        let node = TestSequencer::start(&dir, 1024);
        let history = TestHistory::start(&node, &dir, &index).await.unwrap();
        node.submit(deposit(1, "dep-1")).await;
        node.stop().await;
        history.finish().await.unwrap();
        let mut commands = log_entries(&dir).into_iter().filter_map(|entry| match entry {
            LogEntry::Command(command) => Some(command.sequenced.seq),
            _ => None,
        });
        let last = commands.next_back().expect("the log holds commands");

        let mut cfg = dev_config();
        (cfg.node.log_dir, cfg.history.dir, cfg.history.retention) = (dir, index, RETENTION);
        let (_guard, mut alerts) = capture_alerts();
        let (_, history) = read_log_and_history(&cfg).unwrap();
        assert_eq!(history.resume_at(), Offset::start_of(last + 1));
        assert!(alerts.try_recv().is_err(), "no alert");
    }

    /// A power loss that keeps what the history index took but not the log's unsynced tail leaves
    /// the index ahead of the log. The node discards the index with a Warning alert and goes on to
    /// start: here as far as binding the API, whose port the test holds.
    #[tokio::test]
    async fn a_history_index_ahead_of_its_log_is_discarded_and_the_node_starts() {
        let (kept, index) = index_ahead_of_log("node-history-ahead").await;
        let held = net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut cfg = dev_config();
        (cfg.node.log_dir, cfg.history.dir) = (kept.clone(), index.clone());
        cfg.node.listen = held.local_addr().unwrap();
        cfg.metrics.listen = "127.0.0.1:0".parse().unwrap();
        let (_guard, mut alerts) = capture_alerts();
        let (telemetry, _layer) = telemetry();
        let stopped = timeout(Duration::from_secs(10), run(cfg, telemetry)).await.expect("the node stops in time");
        let err = stopped.expect_err("the API port is held");
        assert!(format!("{err:#}").contains("binding"), "{err:#}");
        let alert = alerts.try_recv().expect("an alert");
        assert_eq!((alert["alert"].as_str(), alert["level"].as_str()), ("history_index_discarded", "WARN"));
        // What the node left is a fresh index, which takes the log from its start with no alert.
        let cfg = HistorySection { dir: index, retention: RETENTION };
        let reopened = open_history(&cfg, &kept).unwrap();
        assert_eq!(reopened.resume_at(), Offset::start_of(0));
        assert!(alerts.try_recv().is_err(), "one alert");
    }

    /// An index the node must discard but cannot remove, beside a write-ahead log the host replaced
    /// with a directory, stops the node with a Critical alert before recovery writes to the log.
    #[tokio::test]
    async fn a_history_index_the_node_cannot_discard_stops_it_before_it_writes_to_the_log() {
        let (kept, index) = index_ahead_of_log("node-history-stuck").await;
        fs::create_dir(index.join(format!("{INDEX_FILE}-wal"))).unwrap();
        // A listing a crash cut short, which recovery trims.
        let mut manifest = OpenOptions::new().append(true).open(kept.join(MANIFEST)).unwrap();
        manifest.write_all(br#"{"file":"#).unwrap();
        let before = files(&kept);

        let mut cfg = dev_config();
        (cfg.node.log_dir, cfg.history.dir) = (kept.clone(), index);
        cfg.node.listen = "127.0.0.1:0".parse().unwrap();
        cfg.metrics.listen = "127.0.0.1:0".parse().unwrap();
        let (_guard, mut alerts) = capture_alerts();
        let (telemetry, _layer) = telemetry();
        let refused = timeout(Duration::from_secs(10), run(cfg, telemetry)).await.expect("the node stops in time");
        let err = refused.expect_err("the index cannot be discarded");
        assert!(format!("{err:#}").contains("removing"), "{err:#}");
        let levels: Vec<_> = iter::from_fn(|| alerts.try_recv().ok())
            .map(|alert| (alert["alert"].clone(), alert["level"].clone()))
            .collect();
        let expected = [("history_index_discarded", "WARN"), ("history_recovery_failed", "ERROR")];
        assert_eq!(levels, expected.map(|(alert, level)| (alert.to_owned(), level.to_owned())));
        assert_eq!(files(&kept), before, "the log is as it was found");
    }

    #[tokio::test]
    async fn a_genesis_cut_short_stops_the_node_before_it_writes_to_the_log() {
        let (dir, index) = (scratch_dir("node-genesis-cut"), scratch_dir("node-genesis-cut-index"));
        TestSequencer::start(&dir, 1024).stop().await;
        // As if the boot that ran genesis died before its house capital credit reached disk.
        let segment = files(&dir).into_keys().filter(|name| segment_seq(name).is_some()).max().expect("a segment");
        let text = fs::read_to_string(dir.join(&segment)).unwrap();
        let credit = text.find(r#""cmd":{"type":"house_capital""#).expect("the credit is in the last segment");
        let line = text[..credit].rfind('\n').expect("a command before it");
        fs::write(dir.join(&segment), &text[..line + 1]).unwrap();
        let before = files(&dir);

        let mut cfg = dev_config();
        (cfg.node.log_dir, cfg.history.dir) = (dir.clone(), index);
        cfg.node.listen = "127.0.0.1:0".parse().unwrap();
        cfg.metrics.listen = "127.0.0.1:0".parse().unwrap();
        let (telemetry, _layer) = telemetry();
        let refused = timeout(Duration::from_secs(10), run(cfg, telemetry)).await.expect("the node stops in time");
        let err = refused.expect_err("genesis did not finish");
        assert!(format!("{err:#}").contains("did not reach the checkpoint after its house capital credit"), "{err:#}");
        assert_eq!(files(&dir), before, "the log is as it was found");
    }

    /// A genesis the engine would reject, in a config that skipped validation, stops the node with a
    /// Critical alert before anything reaches the log.
    #[tokio::test]
    async fn a_genesis_the_engine_would_reject_stops_the_node_with_an_alert_and_writes_nothing() {
        let (dir, index) = (scratch_dir("node-genesis-refused"), scratch_dir("node-genesis-refused-index"));
        let mut cfg = dev_config();
        (cfg.node.log_dir, cfg.history.dir) = (dir.clone(), index);
        cfg.node.listen = "127.0.0.1:0".parse().unwrap();
        cfg.genesis.benchmarks[0].annual_bps = 40_000;
        cfg.metrics.listen = "127.0.0.1:0".parse().unwrap();
        let (_guard, mut alerts) = capture_alerts();
        let (telemetry, _layer) = telemetry();
        let refused = timeout(Duration::from_secs(10), run(cfg, telemetry)).await.expect("the node stops in time");
        let err = refused.expect_err("the engine rejects the benchmark");
        assert!(format!("{err:#}").contains("genesis benchmark SOFR: the engine rejects it"), "{err:#}");
        let raised: Vec<_> = iter::from_fn(|| alerts.try_recv().ok())
            .map(|alert| (alert["alert"].clone(), alert["level"].clone()))
            .collect();
        assert_eq!(raised, [("genesis_refused".to_owned(), "ERROR".to_owned())]);
        assert!(log_entries(&dir).is_empty(), "nothing reached the log");
    }

    // ── Helpers ──

    /// Telemetry with no webhooks, and the layer that feeds it, kept so the router stays open; the
    /// test's own subscriber sees the alert lines.
    fn telemetry() -> (Telemetry, AlertLayer) {
        let metrics = Arc::new(Metrics::new());
        let (layer, alerts) = alerts::channel(Routes::default(), metrics.clone());
        (Telemetry { metrics, alerts, pages_critical: false }, layer)
    }

    /// A log in a new directory named after `name`, as the disk keeps it when the power fails after
    /// its first deposit, and an index that took a second deposit the log then lost.
    async fn index_ahead_of_log(name: &str) -> (PathBuf, PathBuf) {
        let (dir, index) = (scratch_dir(name), scratch_dir(&format!("{name}-index")));
        let node = TestSequencer::start(&dir, 1024);
        let history = TestHistory::start(&node, &dir, &index).await.unwrap();
        node.submit(deposit(1, "dep-1")).await;
        let kept = scratch_dir(&format!("{name}-kept"));
        fs::create_dir_all(&kept).unwrap();
        for file in fs::read_dir(&dir).unwrap() {
            let file = file.unwrap();
            fs::copy(file.path(), kept.join(file.file_name())).unwrap();
        }
        node.submit(deposit(1, "dep-2")).await;
        node.stop().await;
        history.finish().await.unwrap();
        (kept, index)
    }

    /// Every file in `dir` and its bytes.
    fn files(dir: &Path) -> BTreeMap<String, Vec<u8>> {
        let names = fs::read_dir(dir).unwrap().map(|file| file.unwrap().file_name().into_string().unwrap());
        names.map(|name| (name.clone(), fs::read(dir.join(name)).unwrap())).collect()
    }
}
