//! The dev scenario offline: loopback Coinbase, Kraken, and Bitstamp venues price BTC-USD and
//! ETH-USD through the sequencer under the committed dev genesis. With one venue unreachable the
//! markets keep filling; with only one venue left, orders get `no_price`.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use engine_core::command::{Command, OrderKind, OrderRequest};
use engine_core::event::{Event, Reject};
use engine_core::price::PriceError;
use engine_core::types::{AccountId, MarketId, Side};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{interval, timeout};

use crate::clock::now_ms;
use crate::config::{NodeConfig, Venue};
use crate::feeds::tests::fake::{
    WAIT, accept, bitstamp_book, coinbase_levels, fast, kraken_ticker, listen, send, spawn_feed_into,
};
use crate::sequencer::{Request, Sequencer, SequencerConfig, StreamMsg};
use crate::tee::EnclaveKey;

const DEV: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/dev.toml");
/// Each fake venue's book: BTC-USD 81250 / 81251 and ETH-USD 2377 / 2377.1.
const BTC: (&str, &str, &str) = ("81250", "81251", "81250.5");
const ETH: (&str, &str, &str) = ("2377", "2377.1", "2377.05");

/// A sequencer under the dev genesis, and a feed of its stream.
struct Node {
    tx: mpsc::Sender<Request>,
    stream: broadcast::Receiver<Arc<StreamMsg>>,
    account: AccountId,
}

#[tokio::test]
async fn a_blocked_venue_leaves_the_markets_priced_by_the_rest() {
    let mut node = Node::start("blocked-one").await;
    start_venue(&node, Venue::Coinbase, Some(serve_coinbase)).await;
    start_venue(&node, Venue::Kraken, None).await;
    start_venue(&node, Venue::Bitstamp, Some(serve_bitstamp)).await;
    node.wait_for_quotes(&[(1, "coinbase"), (1, "bitstamp"), (2, "coinbase"), (2, "bitstamp")]).await;
    node.deposit().await;

    for (market, (_, _, mid)) in [(1, BTC), (2, ETH)] {
        let events = node.buy(market, mid).await;
        let fill = events.iter().find_map(|event| if let Event::Fill(fill) = event { Some(fill) } else { None });
        let fill = fill.unwrap_or_else(|| panic!("market {market} did not fill: {events:?}"));
        assert_eq!(fill.index, mid.parse().unwrap());
    }
}

#[tokio::test]
async fn with_one_fresh_venue_orders_get_no_price() {
    let mut node = Node::start("blocked-two").await;
    start_venue(&node, Venue::Coinbase, None).await;
    start_venue(&node, Venue::Kraken, Some(serve_kraken)).await;
    start_venue(&node, Venue::Bitstamp, None).await;
    node.wait_for_quotes(&[(1, "kraken"), (2, "kraken")]).await;
    node.deposit().await;

    for (market, (_, _, mid)) in [(1, BTC), (2, ETH)] {
        let events = node.buy(market, mid).await;
        let stale = Reject::NoPrice(PriceError::Stale { fresh: 1, needed: 2 });
        assert!(
            events.iter().any(|event| matches!(event, Event::OrderRejected { reason, .. } if *reason == stale)),
            "market {market}: {events:?}"
        );
    }
}

// ── Helpers ──

impl Node {
    /// Sequences the committed dev genesis into a fresh log and runs the sequencer.
    async fn start(name: &str) -> Self {
        let cfg = NodeConfig::load(Path::new(DEV)).expect("dev config");
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
        while !missing.is_empty() {
            let message = timeout(WAIT, self.stream.recv()).await.expect("quotes in time").expect("stream");
            if let StreamMsg::Command { sequenced, .. } = message.as_ref()
                && let Command::Prices { market, quotes } = &sequenced.cmd
            {
                quotes.iter().for_each(|quote| {
                    missing.remove(&(*market, quote.source.clone()));
                });
            }
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

/// Starts `venue`'s feed into `node`. With `serve`, a loopback venue streams it fresh quotes;
/// without, its URL points at a port nothing listens on.
async fn start_venue(node: &Node, venue: Venue, serve: Option<fn(TcpListener) -> JoinHandle<()>>) {
    let symbols: [(MarketId, &str); 2] = match venue {
        Venue::Bitstamp => [(1, "btcusd"), (2, "ethusd")],
        Venue::Coinbase => [(1, "BTC-USD"), (2, "ETH-USD")],
        Venue::Kraken => [(1, "BTC/USD"), (2, "ETH/USD")],
    };
    let (listener, url) = listen().await;
    match serve {
        Some(serve) => drop(serve(listener)),
        None => drop(listener),
    }
    spawn_feed_into(node.tx.clone(), venue, &url, &symbols, fast(WAIT));
}

/// Serves Kraken tickers for both markets every 20ms, stamped now.
fn serve_kraken(listener: TcpListener) -> JoinHandle<()> {
    tokio::spawn(async move {
        let (mut ws, _) = accept(&listener).await;
        let mut every = interval(Duration::from_millis(20));
        loop {
            every.tick().await;
            let ts = now_ms();
            let tickers = [kraken_ticker("BTC/USD", BTC.0, BTC.1, ts), kraken_ticker("ETH/USD", ETH.0, ETH.1, ts)];
            send(&mut ws, &tickers).await;
        }
    })
}

/// Serves a Coinbase snapshot of both books, then refreshes their levels every 20ms.
fn serve_coinbase(listener: TcpListener) -> JoinHandle<()> {
    tokio::spawn(async move {
        let (mut ws, _) = accept(&listener).await;
        let mut every = interval(Duration::from_millis(20));
        for sequence in (0..).step_by(2) {
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
