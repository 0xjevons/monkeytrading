//! Shared test harness: scratch directories, the committed dev config, and a running sequencer.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use engine_core::command::Command;
use engine_core::event::{CancelReason, Event, Fill, Reject};
use engine_core::price::PriceError;
use engine_core::types::{AccountId, Side};
use engine_core::{Engine, Fixed};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::clock::now_ms;
use crate::config::NodeConfig;
use crate::log::{LogReader, Manifest, Offset};
use crate::sequencer::{Request, Sequencer, SequencerConfig, SignedWithdrawal, StreamMsg};
use crate::tee::EnclaveKey;

pub const DEV_CONFIG: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/dev.toml");
/// Small, so a short test crosses several segment boundaries.
pub const SEGMENT_COMMANDS: u64 = 4;
pub const CHECKPOINT_EVERY: u64 = 3;
pub const TRADER: AccountId = AccountId([7; 20]);

/// A sequencer task running on a log directory, with the ends of its channels.
pub struct TestSequencer {
    pub tx: mpsc::Sender<Request>,
    pub stream: broadcast::Sender<Arc<StreamMsg>>,
    pub manifest: watch::Receiver<Manifest>,
    /// What custody receives.
    pub withdrawals: mpsc::UnboundedReceiver<SignedWithdrawal>,
    task: JoinHandle<anyhow::Result<()>>,
}

impl TestSequencer {
    /// Recovers a sequencer from `dir`, sequences the dev genesis if the log is empty, and runs it.
    /// The stream channel holds `stream_capacity` messages.
    pub fn start(dir: &Path, stream_capacity: usize) -> Self {
        let (stream, _) = broadcast::channel(stream_capacity);
        let (withdrawals_tx, withdrawals) = mpsc::unbounded_channel();
        let (manifest_tx, manifest) = watch::channel(Manifest::default());
        let key = Arc::new(EnclaveKey::generate());
        let mut seq = Sequencer::recover(sequencer_config(dir, manifest_tx), key, stream.clone(), withdrawals_tx)
            .expect("sequencer recovers");
        if seq.needs_genesis() {
            seq.genesis(&dev_config().genesis).expect("dev genesis");
        }
        let (tx, rx) = mpsc::channel(64);
        let task = tokio::spawn(seq.run(rx));
        Self { tx, stream, manifest, withdrawals, task }
    }

    /// Stops the sequencer as a shutdown does: it drains and writes a final checkpoint.
    pub async fn stop(self) {
        drop(self.tx);
        self.task.await.expect("sequencer task").expect("sequencer drains");
    }

    /// Sequences `cmd` and waits until it is flushed and published.
    pub async fn submit(&self, cmd: Command) -> Vec<Event> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(Request::Submit { cmd, reply: Some(reply) }).await.expect("sequencer running");
        rx.await.expect("sequencer replied")
    }

    /// The seq the next command will get.
    pub async fn next_seq(&self) -> u64 {
        let (reply, rx) = oneshot::channel();
        let query = Box::new(move |e: &Engine| {
            let _ = reply.send(e.last_seq().map_or(0, |s| s + 1));
        });
        self.tx.send(Request::Query(query)).await.expect("sequencer running");
        rx.await.expect("sequencer replied")
    }

    /// Sequences ticks until the next command will be followed by a checkpoint, and returns its
    /// seq.
    pub async fn tick_to_checkpoint(&self) -> u64 {
        loop {
            let seq = self.next_seq().await;
            if (seq + 1).is_multiple_of(CHECKPOINT_EVERY) {
                return seq;
            }
            self.submit(Command::Tick).await;
        }
    }
}

/// An empty directory for one test, named after it.
pub fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("engine-node-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// The committed dev config.
pub fn dev_config() -> NodeConfig {
    NodeConfig::load(Path::new(DEV_CONFIG)).expect("dev config loads")
}

/// The sequencer settings the harness runs with, publishing the manifest on `manifest`.
pub fn sequencer_config(dir: &Path, manifest: watch::Sender<Manifest>) -> SequencerConfig<'_> {
    SequencerConfig {
        log_dir: dir,
        segment_commands: SEGMENT_COMMANDS,
        manifest,
        checkpoint_every: CHECKPOINT_EVERY,
        chain_id: 31337,
        vault: AccountId([0; 20]),
        clock: now_ms,
    }
}

/// A deposit to `TRADER` of `amount` whole units under `chain_ref`.
pub fn deposit(amount: i64, chain_ref: &str) -> Command {
    Command::Deposit { account: TRADER, amount: Fixed::from_int(amount), chain_ref: chain_ref.to_owned() }
}

/// A withdrawal by `TRADER` of `amount` whole units.
pub fn withdraw(amount: i64) -> Command {
    Command::Withdraw { account: TRADER, amount: Fixed::from_int(amount), auth: None }
}

/// Every message on disk at or after `from`, as the stream encodes them.
pub fn log_messages(dir: &Path, from: Offset) -> Vec<String> {
    let manifest = watch::channel(Manifest::load(dir).expect("manifest loads")).1;
    let mut reader = LogReader::open(dir, from, manifest).expect("log opens");
    std::iter::from_fn(|| reader.next_entry().expect("log reads"))
        .map(|(offset, entry)| serde_json::to_string(&StreamMsg::new(offset, entry)).expect("message encodes"))
        .collect()
}

/// One event of each kind, and an order rejection for each reason. The matches stop compiling when
/// a variant is added, until it has a sample here.
pub fn every_event() -> Vec<Event> {
    let (account, amount, market) = (AccountId([1; 20]), Fixed::from_int(5), 2);
    let reasons = [
        Reject::UnknownMarket,
        Reject::UnknownOrder,
        Reject::InvalidOrder { reason: "qty".into() },
        Reject::MarketClosed,
        Reject::ReduceOnly,
        Reject::NoPrice { error: PriceError::Stale { fresh: 1, needed: 2 } },
        Reject::NoPrice { error: PriceError::Divergent { divergence_bps: 30 } },
        Reject::Slippage { quoted: amount, live: amount, slippage_bps: 5 },
        Reject::InsufficientMargin { equity: amount, required: amount },
        Reject::InsufficientFunds { available: amount },
        Reject::HouseCapacity,
        Reject::InvalidAmount,
        Reject::DuplicateReference,
        Reject::MissingSignature,
        Reject::InvalidSignature,
        Reject::InvalidNonce { expected: 3 },
        Reject::Expired { expires_at: 1_000 },
        Reject::ExpiryTooFar { latest: 2_000 },
    ];
    for reason in &reasons {
        match reason {
            Reject::UnknownMarket
            | Reject::UnknownOrder
            | Reject::InvalidOrder { .. }
            | Reject::MarketClosed
            | Reject::ReduceOnly
            | Reject::NoPrice { .. }
            | Reject::Slippage { .. }
            | Reject::InsufficientMargin { .. }
            | Reject::InsufficientFunds { .. }
            | Reject::HouseCapacity
            | Reject::InvalidAmount
            | Reject::DuplicateReference
            | Reject::MissingSignature
            | Reject::InvalidSignature
            | Reject::InvalidNonce { .. }
            | Reject::Expired { .. }
            | Reject::ExpiryTooFar { .. } => {}
        }
    }
    let fill = Fill {
        account,
        order_id: 3,
        market,
        side: Side::Sell,
        qty: amount,
        price: amount,
        index: amount,
        spread_bps: 4,
        realised_pnl: -amount,
        profit_fee: Fixed::ZERO,
        position: amount,
        liquidation: true,
    };
    let mut events: Vec<_> =
        reasons.into_iter().map(|reason| Event::OrderRejected { account, client_id: 1, reason }).collect();
    events.extend([
        Event::Deposited { account, amount },
        Event::DepositRejected { account, amount, chain_ref: "0xab:1".into(), reason: Reject::DuplicateReference },
        Event::WithdrawalApproved { account, amount, nonce: 1 },
        Event::WithdrawalRejected { account, amount, reason: Reject::InsufficientFunds { available: amount } },
        Event::HouseCapitalMoved { amount, nav: amount },
        Event::OrderAccepted { account, client_id: 1, order_id: 3 },
        Event::OrderCancelled { account, order_id: 3, reason: CancelReason::Client },
        Event::OrderCancelled { account, order_id: 3, reason: CancelReason::Unfillable },
        Event::Fill(fill),
        Event::CloseOut { account, equity: amount, margin: amount },
        Event::BadDebt { account, amount },
        Event::Financing { account, market, amount: -amount },
        Event::MarketOpened { market },
        Event::MarketClosed { market },
        Event::StressChanged { market, level: 2 },
        Event::PricesRejected { market, reason: "stale quote".into() },
        Event::AdminApplied,
        Event::AdminRejected { reason: "bad params".into() },
    ]);
    for event in &events {
        match event {
            Event::Deposited { .. }
            | Event::DepositRejected { .. }
            | Event::WithdrawalApproved { .. }
            | Event::WithdrawalRejected { .. }
            | Event::HouseCapitalMoved { .. }
            | Event::OrderAccepted { .. }
            | Event::OrderRejected { .. }
            | Event::OrderCancelled { .. }
            | Event::Fill(_)
            | Event::CloseOut { .. }
            | Event::BadDebt { .. }
            | Event::Financing { .. }
            | Event::MarketOpened { .. }
            | Event::MarketClosed { .. }
            | Event::StressChanged { .. }
            | Event::PricesRejected { .. }
            | Event::AdminApplied
            | Event::AdminRejected { .. } => {}
        }
    }
    events
}
