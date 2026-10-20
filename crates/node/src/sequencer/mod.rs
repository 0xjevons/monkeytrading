//! The sequencer owns the engine. Commands arrive over one channel, get a seq and a timestamp, and
//! are applied in order; each then goes into the log with the events it produced. Nothing is
//! acknowledged or published until its log entry is flushed. One task, no locks, and no queue
//! beyond the channel: a fill costs about one engine step plus a buffered write.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use engine_core::auth::ClientAuth;
use engine_core::command::{AdminCommand, Command, Sequenced};
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, Millis};
use engine_core::{Engine, Fixed};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tracing::{error, info, info_span, warn};

use crate::clock::Clock;
use crate::config::Genesis;
use crate::log::{self, Checkpoint, CommandEntry, LogEntry, LogWriter, Manifest, Offset, SegmentedLog};
use crate::tee::EnclaveKey;
use crate::trusted_time::{Sample, TimeGate};

const MAX_BATCH: usize = 1024;
/// The custody reference genesis credits house capital under, as its last command.
const GENESIS_REF: &str = "genesis";

pub type Query = Box<dyn FnOnce(&Engine) + Send>;

// Unboxed on purpose: boxing every command would add an allocation to the hot path.
#[allow(clippy::large_enum_variant)]
pub enum Request {
    Submit {
        cmd: Command,
        reply: Option<oneshot::Sender<Vec<Event>>>,
    },
    Query(Query),
    /// A trusted time sample from the trusted time task.
    TimeSample(Sample),
}

/// What `/v1/stream` publishes: each log entry at its offset, in offset order.
#[derive(Clone, Debug, Serialize)]
pub struct StreamMsg {
    pub offset: Offset,
    #[serde(flatten)]
    pub body: StreamBody,
}

// Unboxed on purpose, like `Request`.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamBody {
    Command(CommandEntry),
    Checkpoint(Checkpoint),
    Withdrawal(SignedWithdrawal),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SignedWithdrawal {
    pub account: AccountId,
    pub amount: Fixed,
    pub nonce: u64,
    pub chain_id: u64,
    pub vault: AccountId,
    pub signature: String,
}

pub struct Sequencer {
    engine: Engine,
    log: LogWriter,
    key: Arc<EnclaveKey>,
    checkpoint_every: u64,
    next_seq: u64,
    last_ts: Millis,
    last_checkpoint: Option<u64>,
    last_offset: Option<Offset>,
    chain_id: u64,
    vault: AccountId,
    stream: broadcast::Sender<Arc<StreamMsg>>,
    withdrawals: mpsc::UnboundedSender<SignedWithdrawal>,
    outbox: Vec<Arc<StreamMsg>>,
    clock: Arc<dyn Clock>,
    time: TimeGate,
    /// Commands the engine must accept, waiting for a stamp trusted time admits, each group with
    /// what it is for.
    required: Vec<(&'static str, Vec<Command>)>,
    client_auth: ClientAuth,
}

pub struct SequencerConfig<'a> {
    pub log_dir: &'a Path,
    pub segment_commands: u64,
    /// Where the log writer publishes its manifest.
    pub manifest: watch::Sender<Manifest>,
    pub checkpoint_every: u64,
    pub chain_id: u64,
    pub vault: AccountId,
    /// The clocks commands are stamped and checked with.
    pub clock: Arc<dyn Clock>,
    /// The most a stamp may differ from trusted time.
    pub max_drift: Duration,
    /// The client auth the config gives, which a log that records one must match.
    pub client_auth: ClientAuth,
}

/// The result of replaying a log.
pub struct Replayed {
    pub engine: Engine,
    pub commands: u64,
    pub checkpoints: u64,
    /// Withdrawals the last command approved that the log ends before signing, as a crash between
    /// a command's entry and its withdrawal entry leaves: `(account, amount, nonce)`.
    pub unsigned: Vec<(AccountId, Fixed, u64)>,
    /// Each protocol version the log activated, with the seq it took effect at.
    pub activations: Vec<(ProtocolVersion, u64)>,
}

impl StreamMsg {
    /// The stream form of the log entry at `offset`.
    pub fn new(offset: Offset, entry: LogEntry) -> Self {
        let body = match entry {
            LogEntry::Command(c) => StreamBody::Command(c),
            LogEntry::Checkpoint(cp) => StreamBody::Checkpoint(cp),
            LogEntry::Withdrawal(w) => StreamBody::Withdrawal(w),
        };
        Self { offset, body }
    }
}

impl Sequencer {
    /// Rebuilds state by replaying the existing log, if there is one, and opens it for appending.
    /// Signs, logs, and hands to custody any withdrawal the log ends before signing.
    ///
    /// TODO: the log sits on host storage, so the host could truncate it. Before serving, check the
    /// replayed root against the latest root posted on-chain.
    /// Error cases:
    /// - the log cannot be read, or fails its manifest check (see `SegmentedLog::open`) or replay
    ///   (see `replay`)
    /// - the log records a client auth other than the config's
    /// - the log cannot be opened for appending, or written
    pub fn recover(
        cfg: SequencerConfig<'_>,
        key: Arc<EnclaveKey>,
        stream: broadcast::Sender<Arc<StreamMsg>>,
        withdrawals: mpsc::UnboundedSender<SignedWithdrawal>,
    ) -> anyhow::Result<Self> {
        let log = SegmentedLog::open(cfg.log_dir)?;
        let Replayed { engine, commands, checkpoints, unsigned, .. } = replay(&log.entries)?;
        // A config naming another client auth, and so another chain or vault, is the wrong one for
        // this log: refuse before anything is written or signed under it.
        if let Some(recorded) = engine.client_auth().filter(|recorded| **recorded != cfg.client_auth) {
            bail!("the log records client auth {recorded:?}, but the config gives {:?}", cfg.client_auth);
        }
        let last_offset = log::last_offset(&log.entries)?;
        info!(commands, checkpoints, "recovered from log");
        let mut sequencer = Self {
            next_seq: engine.last_seq().map_or(0, |s| s + 1),
            last_ts: engine.now(),
            last_checkpoint: engine.last_seq(),
            last_offset,
            engine,
            log: log.into_writer(cfg.segment_commands.max(1), cfg.manifest)?,
            key,
            checkpoint_every: cfg.checkpoint_every.max(1),
            chain_id: cfg.chain_id,
            vault: cfg.vault,
            stream,
            withdrawals,
            outbox: Vec::new(),
            time: TimeGate::new(cfg.max_drift, cfg.clock.instant()),
            clock: cfg.clock,
            required: Vec::new(),
            client_auth: cfg.client_auth,
        };
        for (account, amount, nonce) in unsigned {
            let _span = info_span!("command", id = sequencer.engine.last_seq()).entered();
            warn!(%account, nonce, "the log ends before this approved withdrawal was signed; signing it now");
            sequencer.sign_withdrawal(account, amount, nonce)?;
        }
        sequencer.flush()?;
        Ok(sequencer)
    }

    /// Sequences requests until every sender is gone, then writes a final checkpoint. Each batch is
    /// flushed before its replies, stream messages, and queries go out. A command trusted time
    /// refuses is dropped with its reply, which the API answers as unavailable.
    /// Error cases:
    /// - the log cannot be written; the node must stop rather than acknowledge undurable commands
    /// - the engine rejects a queued required command
    pub async fn run(mut self, mut rx: mpsc::Receiver<Request>) -> anyhow::Result<()> {
        let mut replies = Vec::new();
        let mut queries: Vec<Query> = Vec::new();
        while let Some(first) = rx.recv().await {
            let mut next = Some(first);
            while let Some(req) = next.take() {
                match req {
                    Request::Submit { cmd, reply } => {
                        if let Some(events) = self.sequence(cmd)?
                            && let Some(reply) = reply
                        {
                            replies.push((reply, events));
                        }
                    }
                    Request::Query(q) => queries.push(q),
                    Request::TimeSample(sample) => self.take_sample(sample)?,
                }
                if replies.len() + queries.len() < MAX_BATCH {
                    next = rx.try_recv().ok();
                }
            }
            self.flush()?;
            replies.drain(..).for_each(|(reply, events)| {
                let _ = reply.send(events);
            });
            queries.drain(..).for_each(|q| q(&self.engine));
        }
        if self.last_checkpoint != self.engine.last_seq() {
            self.checkpoint()?;
        }
        self.flush()?;
        info!(seq = ?self.engine.last_seq(), "sequencer drained");
        Ok(())
    }

    /// Stamps and sequences one command, after any queued required commands. Returns `None`, with
    /// nothing logged, when trusted time refuses the stamp.
    /// Error cases:
    /// - the log cannot be written
    /// - the engine rejects a queued required command
    /// - the engine refuses the sequence, which means this sequencer is broken
    fn sequence(&mut self, cmd: Command) -> anyhow::Result<Option<Vec<Event>>> {
        let Some(ts) = self.admitted_ts() else {
            return Ok(None);
        };
        self.sequence_queued(ts)?;
        self.stamp(ts, cmd).map(Some)
    }

    /// Takes `sample` as the latest trusted time, and sequences any queued required commands it
    /// admits.
    /// Error cases:
    /// - the engine rejects a queued required command
    /// - the log cannot be written
    fn take_sample(&mut self, sample: Sample) -> anyhow::Result<()> {
        self.time.update(sample);
        self.sequence_queued_if_admitted()
    }

    /// `take_sample` outside `run`, flushing what it sequences: how a test gives a sequencer trusted
    /// time before it runs.
    /// Error cases:
    /// - as `take_sample`, or the log cannot be flushed
    #[cfg(test)]
    pub fn trust(&mut self, sample: Sample) -> anyhow::Result<()> {
        self.take_sample(sample)?;
        self.flush()
    }

    /// The stamp for a command now, if trusted time admits it. The host clock is untrusted inside
    /// a TEE: it can't move sequenced time backwards, and nothing is logged unless the clock and the
    /// stamp are within the drift bound of trusted time.
    fn admitted_ts(&mut self) -> Option<Millis> {
        let host_ms = self.clock.now_ms();
        let ts = host_ms.max(self.last_ts);
        self.time.admit(host_ms, ts, self.clock.instant()).then_some(ts)
    }

    /// Applies one command at `ts`, logs it with its events, signs any approved withdrawal, and
    /// checkpoints on schedule. The command's span carries its seq as `id`.
    /// Error cases:
    /// - the log cannot be written
    /// - the engine refuses the sequence, which means this sequencer is broken
    fn stamp(&mut self, ts: Millis, cmd: Command) -> anyhow::Result<Vec<Event>> {
        let sequenced = Sequenced { seq: self.next_seq, ts, cmd };
        let _span = info_span!("command", id = sequenced.seq).entered();
        let events = self.engine.apply(&sequenced).context("sequencer produced an invalid sequence")?;
        self.next_seq += 1;
        self.last_ts = ts;
        self.record(LogEntry::Command(CommandEntry { sequenced, events: Some(events.clone()) }))?;

        for event in &events {
            alert_on(event);
            if let Event::WithdrawalApproved { account, amount, nonce } = event {
                self.sign_withdrawal(*account, *amount, *nonce)?;
            }
        }
        if self.next_seq.is_multiple_of(self.checkpoint_every) {
            self.checkpoint()?;
        }
        Ok(events)
    }

    /// Appends an entry to the log and queues it for the stream at its offset.
    /// Error cases:
    /// - the log cannot be written
    fn record(&mut self, entry: LogEntry) -> anyhow::Result<()> {
        let offset = log::offset_of(self.last_offset, &entry)?;
        self.log.append(&entry)?;
        self.last_offset = Some(offset);
        self.outbox.push(Arc::new(StreamMsg::new(offset, entry)));
        Ok(())
    }

    /// Signs an approved withdrawal and logs it; custody and the stream get it once it is flushed.
    /// Error cases:
    /// - the log cannot be written
    fn sign_withdrawal(&mut self, account: AccountId, amount: Fixed, nonce: u64) -> anyhow::Result<()> {
        let signature = self.key.sign_withdrawal(self.chain_id, &self.vault.0, account, amount, nonce);
        info!(%account, nonce, "withdrawal signed");
        self.record(LogEntry::Withdrawal(SignedWithdrawal {
            account,
            amount,
            nonce,
            chain_id: self.chain_id,
            vault: self.vault,
            signature,
        }))
    }

    /// Logs and publishes a signed state root for the last applied seq.
    ///
    /// TODO: hashing the whole state is O(state). Replace it with an incremental Merkle root before
    /// accounts number in the millions.
    /// Error cases:
    /// - nothing has been applied yet
    /// - the log cannot be written
    fn checkpoint(&mut self) -> anyhow::Result<()> {
        let seq = self.engine.last_seq().context("checkpoint before any command")?;
        let root = self.engine.state_root();
        let cp = Checkpoint {
            seq,
            root: format!("0x{}", hex::encode(root)),
            signature: self.key.sign_checkpoint(self.chain_id, &self.vault.0, seq, &root),
            signer: self.key.address_hex(),
        };
        self.record(LogEntry::Checkpoint(cp))?;
        self.last_checkpoint = Some(seq);
        Ok(())
    }

    /// Makes buffered log entries durable, then publishes what they produced and hands signed
    /// withdrawals to custody.
    /// Error cases:
    /// - the log cannot be written
    fn flush(&mut self) -> anyhow::Result<()> {
        self.log.flush()?;
        for msg in self.outbox.drain(..) {
            if let StreamBody::Withdrawal(w) = &msg.body
                && self.withdrawals.send(w.clone()).is_err()
            {
                let (seq, account, nonce) = (msg.offset.seq, w.account, w.nonce);
                error!(alert = "custody_unavailable", seq, %account, nonce, "signed withdrawal has no custody consumer");
            }
            let _ = self.stream.send(msg);
        }
        Ok(())
    }
}

/// Re-applies a log and checks every checkpoint root, logged event, and signed withdrawal against
/// what the open-source engine produces. The same code serves crash recovery and third-party audit
/// (`engine replay`).
/// Error cases:
/// - a command out of sequence or with time running backwards
/// - logged events that differ from the replayed ones, or a command without logged events after
///   one with them
/// - a withdrawal entry that does not match the next unmatched withdrawal its command approved
/// - a withdrawal approved by a command logged with its events, with no withdrawal entry before
///   the next command or checkpoint
/// - a checkpoint for a seq other than the last applied
/// - a checkpoint root that differs from the replayed state
pub fn replay(entries: &[LogEntry]) -> anyhow::Result<Replayed> {
    let mut engine = Engine::new();
    let (mut commands, mut checkpoints, mut activations) = (0, 0, Vec::new());
    // Withdrawals the last command approved that no withdrawal entry has matched yet. Entries
    // written before events were logged have no withdrawal entries. Once a command carries its
    // events, every later one must, and every approval must be signed.
    let mut approved = VecDeque::new();
    let mut logs_events = false;
    for entry in entries {
        match entry {
            LogEntry::Command(c) => {
                ensure_signed(logs_events, &approved, engine.last_seq())?;
                let seq = c.sequenced.seq;
                if logs_events && c.events.is_none() {
                    bail!("seq {seq} has no logged events, though earlier commands do");
                }
                let version = engine.protocol_version();
                let events = engine.apply(&c.sequenced).with_context(|| format!("applying seq {seq}"))?;
                if engine.protocol_version() != version {
                    activations.push((engine.protocol_version(), seq));
                }
                if c.events.as_ref().is_some_and(|logged| *logged != events) {
                    bail!("events logged at seq {seq} differ from the events replay gives");
                }
                approved = events
                    .into_iter()
                    .filter_map(|e| match e {
                        Event::WithdrawalApproved { account, amount, nonce } => Some((account, amount, nonce)),
                        _ => None,
                    })
                    .collect();
                logs_events = c.events.is_some();
                commands += 1;
            }
            LogEntry::Withdrawal(w) => {
                if approved.pop_front() != Some((w.account, w.amount, w.nonce)) {
                    bail!("withdrawal {} nonce {} was not approved at seq {:?}", w.account, w.nonce, engine.last_seq());
                }
            }
            LogEntry::Checkpoint(cp) => {
                ensure_signed(logs_events, &approved, engine.last_seq())?;
                if engine.last_seq() != Some(cp.seq) {
                    bail!("checkpoint for seq {} found after seq {:?}", cp.seq, engine.last_seq());
                }
                let root = format!("0x{}", hex::encode(engine.state_root()));
                if root != cp.root {
                    bail!("state root mismatch at seq {}: log says {}, replay gives {root}", cp.seq, cp.root);
                }
                checkpoints += 1;
            }
        }
    }
    let unsigned = if logs_events { approved.into() } else { Vec::new() };
    Ok(Replayed { engine, commands, checkpoints, unsigned, activations })
}

/// Checks the last command's approved withdrawals all have their signed entries.
/// Error cases:
/// - `must_sign` and an approval is still unmatched
fn ensure_signed(
    must_sign: bool,
    approved: &VecDeque<(AccountId, Fixed, u64)>,
    seq: Option<u64>,
) -> anyhow::Result<()> {
    match approved.front() {
        Some((account, _, nonce)) if must_sign => {
            bail!("withdrawal {account} nonce {nonce} approved at seq {seq:?} has no signed entry")
        }
        _ => Ok(()),
    }
}

/// The commands that sequence `genesis` into an empty log, in order. The last is the house capital
/// credit under `GENESIS_REF`, a zero one included, which marks genesis done.
pub fn genesis_commands(genesis: &Genesis) -> Vec<Command> {
    let mut cmds = vec![Command::Admin(AdminCommand::SetParams(genesis.params.clone()))];
    cmds.extend(genesis.benchmarks.iter().map(|b| {
        Command::Admin(AdminCommand::SetBenchmarkRate { benchmark: b.name.clone(), annual_bps: b.annual_bps })
    }));
    cmds.extend(genesis.markets.iter().map(|m| Command::Admin(AdminCommand::UpsertMarket(m.clone()))));
    cmds.push(Command::HouseCapital { amount: genesis.house_capital, chain_ref: GENESIS_REF.into() });
    cmds
}

/// Raises an alert for an engine rejection an operator must look at.
fn alert_on(event: &Event) {
    match event {
        // A finalised deposit the engine will not credit leaves user funds in the vault and
        // nothing re-examines it.
        Event::DepositRejected { account, amount, chain_ref, reason } => {
            error!(alert = "deposit_rejected", %account, %amount, chain_ref, ?reason, "deposit not credited");
        }
        Event::PricesRejected { market, reason } => {
            warn!(alert = "prices_rejected", market, reason, "price update refused");
        }
        Event::BadDebt { account, amount } => {
            warn!(alert = "bad_debt", %account, %amount, "loss beyond balance written off against the house");
        }
        _ => {}
    }
}

mod required;

#[cfg(test)]
mod tests;
