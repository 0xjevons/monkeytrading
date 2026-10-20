//! The dev scenario offline: loopback Coinbase, Kraken, and Bitstamp venues price BTC-USD and
//! ETH-USD through the sequencer under the dev genesis. Venues go down one at a time: with one
//! down its quotes age out and the markets fill from the other two; with two down, orders get
//! `no_price`.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use engine_core::command::{Command, OrderKind, OrderRequest};
use engine_core::event::{Event, Reject};
use engine_core::price::{PriceError, SourceQuote};
use engine_core::types::{AccountId, MarketId, Millis, Side};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{interval, sleep, timeout};

use crate::clock::now_ms;
use crate::config::{NodeConfig, Venue};
use crate::feeds::tests::fake::{
    WAIT, accept, bitstamp_book, capture_alerts, coinbase_levels, fast, kraken_book, listen, next_alert, send,
    spawn_feed_into, stream,
};
use crate::feeds::venue::Timing;
use crate::sequencer::{Request, Sequencer, SequencerConfig, StreamMsg};
use crate::tee::EnclaveKey;

const DEV: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/dev.toml");
/// Each fake venue's book: BTC-USD 81250 / 81251 and ETH-USD 2377 / 2377.1.
const BTC: (&str, &str, &str) = ("81250", "81251", "81250.5");
const ETH: (&str, &str, &str) = ("2377", "2377.1", "2377.05");
/// The crypto markets' freshness window in this test's genesis: short so a cut venue ages out
/// within the test, long enough that a busy runner does not age out the live ones.
const MAX_AGE_MS: Millis = 1_000;

/// A sequencer under the dev genesis, and a feed of its stream.
struct Node {
    tx: mpsc::Sender<Request>,
    stream: broadcast::Receiver<Arc<StreamMsg>>,
    account: AccountId,
}

#[tokio::test]
async fn venues_going_down_age_out_and_the_index_runs_on_the_rest() {
    let mut node = Node::start("staged").await;
    let _coinbase = start_venue(&node, Venue::Coinbase, serve_coinbase).await;
    let kraken = start_venue(&node, Venue::Kraken, serve_kraken).await;
    let bitstamp = start_venue(&node, Venue::Bitstamp, serve_bitstamp).await;
    let every_quote: Vec<(MarketId, &str)> = [1, 2]
        .into_iter()
        .flat_map(|market| ["coinbase", "kraken", "bitstamp"].map(|source| (market, source)))
        .collect();
    node.wait_for_quotes(&every_quote).await;
    node.deposit().await;
    node.assert_both_markets_fill().await;

    // Kraken's host goes dark: its connection drops, reconnects fail, and its last quotes age out.
    // The sequencer stamps with the wall clock, so ageing takes real time.
    kraken.abort();
    sleep(Duration::from_millis(MAX_AGE_MS as u64 + 500)).await;
    node.assert_both_markets_fill().await;

    // With Bitstamp gone too, one fresh venue is left.
    bitstamp.abort();
    sleep(Duration::from_millis(MAX_AGE_MS as u64 + 500)).await;
    for (market, (_, _, mid)) in [(1, BTC), (2, ETH)] {
        let events = node.buy(market, mid).await;
        let stale = Reject::NoPrice { error: PriceError::Stale { fresh: 1, needed: 2 } };
        assert!(
            events.iter().any(|event| matches!(event, Event::OrderRejected { reason, .. } if *reason == stale)),
            "market {market}: {events:?}"
        );
    }
}

#[tokio::test]
async fn after_a_restart_quotes_older_than_the_engines_do_not_count_as_quoting() {
    let (_guard, mut alerts) = capture_alerts();
    let node = Node::start("restart").await;
    // The engine already holds a Kraken BTC quote, as a replayed log can.
    let held = now_ms();
    let quote = SourceQuote { source: "kraken".into(), mid: BTC.2.parse().unwrap(), ts: held };
    node.submit(Command::Prices { market: 1, quotes: vec![quote] }).await;

    // The restarted feed's BTC quotes are stamped just before it, as from a venue whose clock
    // stepped back, so the engine ignores them. ETH quotes normally.
    let (listener, url) = listen().await;
    tokio::spawn(async move {
        let (ws, _) = accept(&listener).await;
        let books = move || {
            vec![kraken_book("BTC/USD", BTC.0, BTC.1, held - 1), kraken_book("ETH/USD", ETH.0, ETH.1, now_ms())]
        };
        let _ = stream(ws, books).await;
    });
    // A wide freshness bound, so only the engine's rule can keep the BTC quotes from counting.
    let timing = Timing { stale_after: Duration::from_secs(60), ..fast(Duration::from_millis(300)) };
    spawn_feed_into(node.tx.clone(), Venue::Kraken, &url, &[(1, "BTC/USD"), (2, "ETH/USD")], timing);
    let silent = async {
        loop {
            let alert = next_alert(&mut alerts).await;
            if alert["alert"] == "venue_symbol_silent" {
                return alert;
            }
        }
    };
    let alert = timeout(WAIT, silent).await.expect("BTC/USD alerted as silent");
    assert_eq!(alert["symbol"], "BTC/USD");
}

// ── Helpers ──

impl Node {
    /// Sequences the committed dev genesis, with `MAX_AGE_MS` on the crypto markets, into a fresh
    /// log and runs the sequencer.
    async fn start(name: &str) -> Self {
        let mut cfg = NodeConfig::load(Path::new(DEV)).expect("dev config");
        for market in cfg.genesis.markets.iter_mut().filter(|market| [1, 2].contains(&market.id)) {
            market.price_rules.max_age_ms = MAX_AGE_MS;
        }
        let dir = std::env::temp_dir().join(format!("engine-feeds-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let (stream_tx, stream) = broadcast::channel(65_536);
        let (withdrawals_tx, _) = mpsc::unbounded_channel();
        let seq_cfg = SequencerConfig {
            log_path: &dir.join("log.jsonl"),
            checkpoint_every: 1_000,
            chain_id: cfg.custody.chain_id,
            vault: cfg.custody.vault.parse().expect("vault"),
        };
        let mut seq = Sequencer::recover(seq_cfg, Arc::new(EnclaveKey::generate()), stream_tx, withdrawals_tx)
            .expect("fresh sequencer");
        seq.genesis(&cfg.genesis).expect("genesis");
        let (tx, rx) = mpsc::channel(65_536);
        tokio::spawn(seq.run(rx));
        let account = "0x00000000000000000000000000000000000000a1".parse().expect("account");
        Node { tx, stream, account }
    }

    /// Waits until the engine has sequenced a quote from every `(market, source)` pair.
    async fn wait_for_quotes(&mut self, expected: &[(MarketId, &str)]) {
        let mut missing: BTreeSet<(MarketId, String)> =
            expected.iter().map(|(market, source)| (*market, (*source).to_owned())).collect();
        let arrivals = async {
            while !missing.is_empty() {
                let message = self.stream.recv().await.expect("stream");
                if let StreamMsg::Command { sequenced, .. } = message.as_ref()
                    && let Command::Prices { market, quotes } = &sequenced.cmd
                {
                    quotes.iter().for_each(|quote| {
                        missing.remove(&(*market, quote.source.clone()));
                    });
                }
            }
        };
        timeout(WAIT, arrivals).await.expect("a quote from every venue in time");
    }

    /// Buys in both markets and checks each fills at the venues' shared mid.
    async fn assert_both_markets_fill(&self) {
        for (market, (_, _, mid)) in [(1, BTC), (2, ETH)] {
            let events = self.buy(market, mid).await;
            let fill = events.iter().find_map(|event| if let Event::Fill(fill) = event { Some(fill) } else { None });
            let fill = fill.unwrap_or_else(|| panic!("market {market} did not fill: {events:?}"));
            assert_eq!(fill.index, mid.parse().unwrap());
        }
    }

    /// Credits the test account.
    async fn deposit(&self) {
        let cmd =
            Command::Deposit { account: self.account, amount: "100000".parse().unwrap(), chain_ref: "dev-1".into() };
        let events = self.submit(cmd).await;
        assert!(events.iter().any(|event| matches!(event, Event::Deposited { .. })), "{events:?}");
    }

    /// A small market buy quoted at `mid`, and the events it produced.
    async fn buy(&self, market: MarketId, mid: &str) -> Vec<Event> {
        let kind = OrderKind::Market { quoted: mid.parse().unwrap(), max_slippage_bps: None };
        let qty = "0.001".parse().unwrap();
        let order = OrderRequest {
            account: self.account,
            client_id: 1,
            market,
            side: Side::Buy,
            qty,
            kind,
            reduce_only: false,
        };
        self.submit(Command::PlaceOrder(order)).await
    }

    /// Sequences `cmd` and returns its events.
    async fn submit(&self, cmd: Command) -> Vec<Event> {
        let (reply, events) = oneshot::channel();
        self.tx.send(Request::Submit { cmd, reply: Some(reply) }).await.expect("sequencer up");
        timeout(WAIT, events).await.expect("reply in time").expect("reply")
    }
}

/// Starts `venue`'s feed into `node` against a loopback venue that `serve` runs, and returns the
/// venue's task: aborting it takes the venue down.
async fn start_venue(node: &Node, venue: Venue, serve: fn(TcpListener) -> JoinHandle<()>) -> JoinHandle<()> {
    let symbols: [(MarketId, &str); 2] = match venue {
        Venue::Bitstamp => [(1, "btcusd"), (2, "ethusd")],
        Venue::Coinbase => [(1, "BTC-USD"), (2, "ETH-USD")],
        Venue::Kraken => [(1, "BTC/USD"), (2, "ETH/USD")],
    };
    let (listener, url) = listen().await;
    let server = serve(listener);
    spawn_feed_into(node.tx.clone(), venue, &url, &symbols, fast(WAIT));
    server
}

/// Serves Kraken tickers for both markets every 20ms, stamped now.
fn serve_kraken(listener: TcpListener) -> JoinHandle<()> {
    tokio::spawn(async move {
        let (mut ws, _) = accept(&listener).await;
        let mut every = interval(Duration::from_millis(20));
        loop {
            every.tick().await;
            let ts = now_ms();
            let tickers = [kraken_book("BTC/USD", BTC.0, BTC.1, ts), kraken_book("ETH/USD", ETH.0, ETH.1, ts)];
            send(&mut ws, &tickers).await;
        }
    })
}

/// Serves a Coinbase snapshot of both books, then refreshes their levels every 20ms.
fn serve_coinbase(listener: TcpListener) -> JoinHandle<()> {
    tokio::spawn(async move {
        let (mut ws, _) = accept(&listener).await;
        let mut every = interval(Duration::from_millis(20));
        for sequence in (0..u64::MAX).step_by(2) {
            every.tick().await;
            let (ts, snapshot) = (now_ms(), sequence == 0);
            let books = [
                coinbase_levels(sequence, snapshot, "BTC-USD", BTC.0, BTC.1, ts),
                coinbase_levels(sequence + 1, snapshot, "ETH-USD", ETH.0, ETH.1, ts),
            ];
            send(&mut ws, &books).await;
        }
    })
}

/// Serves Bitstamp books for both markets every 20ms, stamped now.
fn serve_bitstamp(listener: TcpListener) -> JoinHandle<()> {
    tokio::spawn(async move {
        let (mut ws, _) = accept(&listener).await;
        let mut every = interval(Duration::from_millis(20));
        loop {
            every.tick().await;
            let ts = now_ms();
            send(&mut ws, &[bitstamp_book("btcusd", BTC.0, BTC.1, ts), bitstamp_book("ethusd", ETH.0, ETH.1, ts)])
                .await;
        }
    })
}
