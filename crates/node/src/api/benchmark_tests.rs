//! Deterministic dev scenario through the real publisher worker, signed admin API, TLS, and log.

use super::*;
use std::fs;
use std::path::Path;
use std::time::Duration;

use clap::Parser;
use engine_core::command::{Command, OrderKind, OrderRequest};
use engine_core::event::Event;
use engine_core::fixed::Rounding;
use engine_core::price::SourceQuote;
use engine_core::types::Side;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::admin::{self, AdminArgs, HexKey, KeyEnv};
use crate::clock::Clock;
use crate::clock::test_clock::TestClock;
use crate::history::Reader;
use crate::limits::ApiLimits;
use crate::log::{LogEntry, Manifest};
use crate::sequencer::{Request as SeqRequest, Sequencer, SequencerConfig, replay};
use crate::tee::{EnclaveKey, MockAttestor, attest_key};
use crate::test_support::{dev_config, log_entries, sample_around, scratch_dir, sequencer_config};
use crate::tls::{TlsIdentity, serve};

const START: Millis = 1_792_504_800_000;
const FIRST: Millis = 1_792_530_000_000; // 20 Oct 2026 21:00 UTC = 22:00 London.
const RESTART: Millis = FIRST + 2 * 86_400_000 + 1;
const TRADER: AccountId = AccountId([0xaa; 20]);

#[derive(Parser)]
struct Cli {
    #[command(flatten)]
    args: AdminArgs,
}

struct Node {
    tx: mpsc::Sender<SeqRequest>,
    task: JoinHandle<anyhow::Result<()>>,
    api: JoinHandle<anyhow::Result<()>>,
    url: String,
    pin: String,
    clock: Arc<TestClock>,
}

#[tokio::test]
async fn dev_scenario_benchmark_worker_catches_up_two_rollovers_at_their_dated_rates() {
    let dir = scratch_dir("benchmark-dev-scenario");
    fs::create_dir_all(&dir).unwrap();
    let (publisher, publishers) = publisher_server().await;
    let cfg_path = dir.join("node.toml");
    let mut text = fs::read_to_string(crate::test_support::DEV_CONFIG).unwrap();
    text.push_str(&format!(
        r#"
[benchmarks]
interval = "1h"
timeout = "2s"
[[benchmarks.sources]]
publisher = "sofr"
endpoint = "{publisher}/sofr"
[[benchmarks.sources]]
publisher = "estr"
endpoint = "{publisher}/estr"
"#
    ));
    fs::write(&cfg_path, text).unwrap();
    let first = Node::start(&dir, START, || START).await;
    // A ticker cannot charge financing before the startup publisher synchronization.
    assert!(first.try_submit(Command::Tick).await.is_none());
    first.sync(&cfg_path, || START).await;
    first
        .submit(Command::Deposit { account: TRADER, amount: fixed("100000"), chain_ref: "benchmark-deposit".into() })
        .await;
    first
        .submit(Command::Prices {
            market: 1,
            quotes: ["venue-a", "venue-b"]
                .into_iter()
                .map(|s| SourceQuote { source: s.into(), mid: fixed("100000"), ts: START })
                .collect(),
        })
        .await;
    let fills = first
        .submit(Command::PlaceOrder(OrderRequest {
            account: TRADER,
            client_id: 1,
            market: 1,
            side: Side::Buy,
            qty: fixed("0.1"),
            kind: OrderKind::Market { quoted: fixed("100000"), max_slippage_bps: None },
            reduce_only: false,
            auth: None,
        }))
        .await;
    assert!(fills.iter().any(|e| matches!(e, Event::Fill(_))), "{fills:?}");
    first.advance(FIRST + 1).await;
    let events = first.submit(Command::Tick).await;
    assert_eq!(charges(&events), [charge("0.01")]);
    first.stop().await;

    // The publisher fixture includes the intervening rate changes. On restart it is fetched
    // before ticks, deposits, or feeds can cause catch-up to use the pre-downtime snapshot.
    let restarted = Node::start(&dir, RESTART, || RESTART).await;
    assert!(restarted.try_submit(Command::Tick).await.is_none());
    let request = restarted.sync(&cfg_path, || RESTART).await;
    assert!(request.command.is_benchmark_snapshot());
    let events = restarted.submit(Command::Tick).await;
    assert!(charges(&events).is_empty(), "snapshot already charged both missed rollovers");
    restarted.stop().await;
    publishers.abort();

    let entries = log_entries(&dir);
    let financing: Vec<_> = entries
        .iter()
        .filter_map(|entry| match entry {
            LogEntry::Command(c) => c.events.as_deref(),
            _ => None,
        })
        .flat_map(charges)
        .collect();
    assert_eq!(financing, [charge("0.01"), charge("0.015"), charge("0.02003")]);
    let replayed = replay(&entries).unwrap();
    assert_eq!(replayed.engine.accounts()[&TRADER].balance, fixed("100000") - financing.into_iter().sum::<Fixed>());
    assert!(entries.iter().any(|entry| matches!(entry, LogEntry::Command(c) if c.sequenced.cmd == Command::SignedAdmin(Box::new(request.clone())))));
}

impl Node {
    async fn start(dir: &Path, at: Millis, api_clock: fn() -> Millis) -> Self {
        let clock = Arc::new(TestClock::at(at));
        let key = Arc::new(EnclaveKey::generate());
        let (stream, _) = broadcast::channel(1024);
        let (manifest_tx, manifest) = watch::channel(Manifest::default());
        let (withdrawals, _) = mpsc::unbounded_channel();
        let cfg = SequencerConfig { clock: clock.clone(), ..sequencer_config(dir, manifest_tx) };
        let operator = cfg.operator;
        let mut seq = Sequencer::recover(cfg, key.clone(), stream.clone(), withdrawals).unwrap();
        seq.wait_for_benchmarks();
        seq.activate_protocol().unwrap();
        if seq.needs_genesis() {
            seq.genesis(&dev_config().genesis).unwrap();
        }
        seq.trust(sample_around(clock.as_ref())).unwrap();
        let (tx, rx) = mpsc::channel(1024);
        let task = tokio::spawn(seq.run(rx));
        let tls = TlsIdentity::generate().unwrap();
        let state = ApiState {
            tx: tx.clone(),
            stream,
            attestation: Arc::new(attest_key(&MockAttestor, &key, &tls.public_key_sha256).unwrap()),
            auth: ClientAuth::Unsigned,
            operator,
            mock_custody: true,
            clock: api_clock,
            log_dir: dir.into(),
            manifest,
            backfills: Arc::new(Semaphore::new(MAX_BACKFILLS)),
            limits: Arc::new(ApiLimits::new(&dev_config().api)),
            history: Reader::new(PathBuf::new(), watch::channel(true).1),
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("https://{}", listener.local_addr().unwrap());
        let app = router(state.clone());
        let api = tokio::spawn(serve(listener, tls.config, app, state.limits.clone(), dev_config().api));
        Self { tx, task, api, url, pin: hex::encode(tls.public_key_sha256), clock }
    }

    async fn sync(&self, config: &Path, clock: fn() -> Millis) -> engine_core::command::AdminRequest {
        let args = Cli::try_parse_from([
            "engine",
            "--config",
            config.to_str().unwrap(),
            "--node",
            &self.url,
            "--pin",
            &self.pin,
            "sync-benchmarks",
        ])
        .unwrap()
        .args;
        let env = KeyEnv { key_file: None, key: Some(HexKey("42".repeat(32))) };
        admin::run_with_clock(args, env, clock).await.unwrap()
    }

    async fn try_submit(&self, cmd: Command) -> Option<Vec<Event>> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(SeqRequest::Submit { cmd, reply: Some(reply) }).await.unwrap();
        rx.await.ok()
    }

    async fn submit(&self, cmd: Command) -> Vec<Event> {
        self.try_submit(cmd).await.expect("sequenced command")
    }

    async fn advance(&self, to: Millis) {
        self.clock.advance(Duration::from_millis((to - self.clock.now_ms()) as u64));
        self.tx.send(SeqRequest::TimeSample(sample_around(self.clock.as_ref()))).await.unwrap();
    }

    async fn stop(self) {
        self.api.abort();
        let _ = self.api.await;
        drop(self.tx);
        self.task.await.unwrap().unwrap();
    }
}

async fn publisher_server() -> (String, JoinHandle<std::io::Result<()>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route(
            "/sofr",
            get(|| async {
                "Effective Date,Rate Type,Rate (%)\n10/20/2026,SOFR,1.00\n10/21/2026,SOFR,1.50\n10/22/2026,SOFR,2.003\n"
            }),
        )
        .route("/estr", get(|| async { "KEY,TIME_PERIOD,OBS_VALUE\nEST.B.EU000A2X2A25.WT,2026-10-20,2.442\n" }));
    (url, tokio::spawn(async move { axum::serve(listener, app).await }))
}

fn fixed(s: &str) -> Fixed {
    s.parse().unwrap()
}
fn charge(rate: &str) -> Fixed {
    fixed("10000").mul_rounded(fixed(rate) + Fixed::from_bps(500), Rounding::Up).div_int_rounded(365, Rounding::Up)
}
fn charges(events: &[Event]) -> Vec<Fixed> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Financing { amount, market: 1, .. } => Some(*amount),
            _ => None,
        })
        .collect()
}
