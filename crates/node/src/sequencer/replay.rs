//! Log replay: re-applies a log and checks it against what the open-source engine produces.

use std::collections::VecDeque;

use anyhow::{Context, bail};
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;
use engine_core::{Engine, Fixed};

use crate::log::LogEntry;

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
