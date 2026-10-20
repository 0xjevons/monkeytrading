//! Shared test harness: scratch directories, the committed dev config, and a running sequencer.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use engine_core::Fixed;
use engine_core::command::Command;
use engine_core::event::Event;
use engine_core::types::AccountId;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::config::NodeConfig;
use crate::log::{LogReader, Offset};
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
    /// Kept open so custody hand-offs succeed.
    _withdrawals: mpsc::UnboundedReceiver<SignedWithdrawal>,
    task: JoinHandle<anyhow::Result<()>>,
}

impl TestSequencer {
    /// Recovers a sequencer from `dir`, sequences the dev genesis if the log is empty, and runs it.
    /// The stream channel holds `stream_capacity` messages.
    pub fn start(dir: &Path, stream_capacity: usize) -> Self {
        let (stream, _) = broadcast::channel(stream_capacity);
        let (withdrawals_tx, _withdrawals) = mpsc::unbounded_channel();
        let mut seq =
            Sequencer::recover(sequencer_config(dir), Arc::new(EnclaveKey::generate()), stream.clone(), withdrawals_tx)
                .expect("sequencer recovers");
        if seq.is_fresh() {
            seq.genesis(&dev_config().genesis).expect("dev genesis");
        }
        let (tx, rx) = mpsc::channel(64);
        let task = tokio::spawn(seq.run(rx));
        Self { tx, stream, _withdrawals, task }
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
        let query = Box::new(move |e: &engine_core::Engine| {
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

/// The sequencer settings the harness runs with.
pub fn sequencer_config(dir: &Path) -> SequencerConfig<'_> {
    SequencerConfig {
        log_dir: dir,
        segment_commands: SEGMENT_COMMANDS,
        checkpoint_every: CHECKPOINT_EVERY,
        chain_id: 31337,
        vault: AccountId([0; 20]),
    }
}

/// A deposit to `TRADER` of `amount` whole units under `chain_ref`.
pub fn deposit(amount: i64, chain_ref: &str) -> Command {
    Command::Deposit { account: TRADER, amount: Fixed::from_int(amount), chain_ref: chain_ref.to_owned() }
}

/// A withdrawal by `TRADER` of `amount` whole units.
pub fn withdraw(amount: i64) -> Command {
    Command::Withdraw { account: TRADER, amount: Fixed::from_int(amount) }
}

/// Every message on disk at or after `from`, as the stream encodes them.
pub fn log_messages(dir: &Path, from: Offset) -> Vec<String> {
    let mut reader = LogReader::open(dir, from).expect("log opens");
    std::iter::from_fn(|| reader.next_entry().expect("log reads"))
        .map(|(offset, entry)| serde_json::to_string(&StreamMsg::new(offset, entry)).expect("message encodes"))
        .collect()
}
