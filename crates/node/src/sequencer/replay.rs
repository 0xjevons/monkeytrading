//! Log replay: re-applies a log as it is read and checks it against what the open-source engine
//! produces.

use std::collections::VecDeque;
use std::fs;
use std::path::Path;

use anyhow::{Context, bail, ensure};
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, Millis};
use engine_core::{Engine, Fixed};

use crate::log::{self, LogEntry, SegmentedLog};
use crate::sequencer::GenesisProgress;
use crate::snapshot::{Progress, Snapshot};

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
    /// How far the log got through genesis.
    pub genesis: GenesisProgress,
    /// Sequenced time of the command the last checkpoint covers.
    pub last_checkpoint_ts: Option<Millis>,
}

/// Re-applies a log entry by entry, as it is read, and checks every checkpoint root, logged event,
/// and signed withdrawal against what the open-source engine produces. The same code serves crash
/// recovery and third-party audit (`engine replay`).
#[derive(Default)]
pub(super) struct Replay {
    engine: Engine,
    commands: u64,
    checkpoints: u64,
    activations: Vec<(ProtocolVersion, u64)>,
    /// Withdrawals the last command approved that no withdrawal entry has matched yet. Entries
    /// written before events were logged have no withdrawal entries. Once a command carries its
    /// events, every later one must, and every approval must be signed.
    approved: VecDeque<(AccountId, Fixed, u64)>,
    logs_events: bool,
    genesis: GenesisProgress,
    last_checkpoint_ts: Option<Millis>,
}

impl Replay {
    /// Resumes only a verified, completely signed checkpoint, preserving replay's prefix facts.
    pub(super) fn from_snapshot(engine: Engine, snapshot: Snapshot) -> Self {
        let Snapshot { progress: Progress { genesis, activations, checkpoints }, .. } = snapshot;
        let commands = engine.last_seq().expect("verified snapshot has a sequence") + 1;
        let last_checkpoint_ts = Some(engine.now());
        Self {
            engine,
            commands,
            checkpoints,
            activations,
            genesis,
            last_checkpoint_ts,
            approved: VecDeque::new(),
            logs_events: true,
        }
    }

    /// Applies the log's next entry.
    /// Error cases:
    /// - a command out of sequence, with time running backwards, or stamped past the range the
    ///   engine accepts
    /// - logged events that differ from the replayed ones, or a command without logged events after
    ///   one with them
    /// - a withdrawal entry that does not match the next unmatched withdrawal its command approved
    /// - a withdrawal approved by a command logged with its events, with no withdrawal entry before
    ///   the next command or checkpoint
    /// - a checkpoint for a seq other than the last applied
    /// - a checkpoint root that differs from the replayed state
    pub(super) fn apply(&mut self, entry: &LogEntry) -> anyhow::Result<()> {
        let engine = &mut self.engine;
        match entry {
            LogEntry::Command(c) => {
                ensure_signed(self.logs_events, &self.approved, engine.last_seq())?;
                let seq = c.sequenced.seq;
                if self.logs_events && c.events.is_none() {
                    bail!("seq {seq} has no logged events, though earlier commands do");
                }
                let version = engine.protocol_version();
                let events = engine.apply(&c.sequenced).with_context(|| format!("applying seq {seq}"))?;
                self.genesis.command(&c.sequenced, &events);
                if engine.protocol_version() != version {
                    self.activations.push((engine.protocol_version(), seq));
                }
                if c.events.as_ref().is_some_and(|logged| *logged != events) {
                    bail!("events logged at seq {seq} differ from the events replay gives");
                }
                self.approved = events
                    .into_iter()
                    .filter_map(|e| match e {
                        Event::WithdrawalApproved { account, amount, nonce } => Some((account, amount, nonce)),
                        _ => None,
                    })
                    .collect();
                self.logs_events = c.events.is_some();
                self.commands += 1;
            }
            LogEntry::Withdrawal(w) => {
                if self.approved.pop_front() != Some((w.account, w.amount, w.nonce)) {
                    bail!("withdrawal {} nonce {} was not approved at seq {:?}", w.account, w.nonce, engine.last_seq());
                }
            }
            LogEntry::Checkpoint(cp) => {
                ensure_signed(self.logs_events, &self.approved, engine.last_seq())?;
                if engine.last_seq() != Some(cp.seq) {
                    bail!("checkpoint for seq {} found after seq {:?}", cp.seq, engine.last_seq());
                }
                let root = format!("0x{}", hex::encode(engine.state_root()));
                if root != cp.root {
                    bail!("state root mismatch at seq {}: log says {}, replay gives {root}", cp.seq, cp.root);
                }
                self.checkpoints += 1;
                self.genesis.checkpoint();
                // The checkpoint covers the last command applied, whose stamp is the engine's time.
                self.last_checkpoint_ts = Some(engine.now());
            }
        }
        Ok(())
    }

    /// What the entries applied so far replay to.
    pub(super) fn finish(self) -> Replayed {
        let unsigned = if self.logs_events { self.approved.into() } else { Vec::new() };
        let (engine, commands, checkpoints, activations, genesis, last_checkpoint_ts) =
            (self.engine, self.commands, self.checkpoints, self.activations, self.genesis, self.last_checkpoint_ts);
        Replayed { engine, commands, checkpoints, unsigned, activations, genesis, last_checkpoint_ts }
    }
}

/// Replays `entries`, a whole log held in memory (see `Replay`).
/// Error cases:
/// - an entry fails replay (see `Replay::apply`)
pub fn replay(entries: &[LogEntry]) -> anyhow::Result<Replayed> {
    let mut replay = Replay::default();
    entries.iter().try_for_each(|entry| replay.apply(entry))?;
    Ok(replay.finish())
}

/// Replays the log at `path`, a single log file or a directory of segments with their manifest, as
/// it reads it, for `engine replay`. A log that holds no checkpoint verifies nothing, so it fails.
/// Error cases:
/// - nothing is at `path`, or it cannot be read
/// - the log fails its manifest check (see `SegmentedLog::open`) or replay (see `Replay::apply`)
/// - the log holds no command, or no checkpoint
pub fn replay_path(path: &Path) -> anyhow::Result<Replayed> {
    let found = fs::metadata(path).with_context(|| format!("no readable log at {}", path.display()))?;
    let mut replay = Replay::default();
    if found.is_dir() {
        SegmentedLog::open(path, |entry| replay.apply(&entry))?;
    } else {
        log::read_file(path, |entry| replay.apply(&entry))?;
    }
    let replayed = replay.finish();
    ensure!(replayed.commands > 0, "{} holds no command", path.display());
    ensure!(replayed.checkpoints > 0, "{} holds no checkpoint, so nothing in it is verified", path.display());
    Ok(replayed)
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
