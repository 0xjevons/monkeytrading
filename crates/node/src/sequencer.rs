//! The sequencer owns the engine. Commands arrive over one channel, get a seq and a timestamp, go
//! into the log, and are applied in order. Nothing is acknowledged or published until its log
//! entry is flushed. One task, no locks, and no queue beyond the channel: a fill costs about one
//! engine step plus a buffered write.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, bail};
use engine_core::command::{AdminCommand, Command, Sequenced};
use engine_core::event::Event;
use engine_core::types::{AccountId, Millis};
use engine_core::{Engine, Fixed};
use serde::Serialize;
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{error, info, info_span, warn};

use crate::clock::now_ms;
use crate::config::Genesis;
use crate::log::{self, Checkpoint, LogEntry, LogWriter};
use crate::tee::EnclaveKey;

const MAX_BATCH: usize = 1024;

pub type Query = Box<dyn FnOnce(&Engine) + Send>;

// Unboxed on purpose: boxing every command would add an allocation to the hot path.
#[allow(clippy::large_enum_variant)]
pub enum Request {
    Submit { cmd: Command, reply: Option<oneshot::Sender<Vec<Event>>> },
    Query(Query),
}

/// What `/v1/stream` publishes, in sequence order.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamMsg {
    Command {
        #[serde(flatten)]
        sequenced: Sequenced,
        events: Vec<Event>,
    },
    Checkpoint(Checkpoint),
    Withdrawal(SignedWithdrawal),
}

#[derive(Clone, Debug, Serialize)]
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
    chain_id: u64,
    vault: AccountId,
    stream: broadcast::Sender<Arc<StreamMsg>>,
    withdrawals: mpsc::UnboundedSender<SignedWithdrawal>,
    outbox: Vec<Arc<StreamMsg>>,
}

pub struct SequencerConfig<'a> {
    pub log_path: &'a Path,
    pub checkpoint_every: u64,
    pub chain_id: u64,
    pub vault: AccountId,
}

/// The result of replaying a log.
pub struct Replayed {
    pub engine: Engine,
    pub commands: u64,
    pub checkpoints: u64,
}

impl Sequencer {
    /// Rebuilds state by replaying the existing log, if there is one, and opens it for appending.
    ///
    /// TODO: the log sits on host storage, so the host could truncate it. Before serving, check the
    /// replayed root against the latest root posted on-chain.
    /// Error cases:
    /// - the log cannot be read, or fails replay (see `replay`)
    /// - the log cannot be opened for appending
    pub fn recover(
        cfg: SequencerConfig<'_>,
        key: Arc<EnclaveKey>,
        stream: broadcast::Sender<Arc<StreamMsg>>,
        withdrawals: mpsc::UnboundedSender<SignedWithdrawal>,
    ) -> anyhow::Result<Self> {
        let replayed = replay(&log::read_all(cfg.log_path)?)?;
        info!(commands = replayed.commands, checkpoints = replayed.checkpoints, "recovered from log");
        Ok(Self {
            next_seq: replayed.engine.last_seq().map_or(0, |s| s + 1),
            last_ts: replayed.engine.now(),
            last_checkpoint: replayed.engine.last_seq(),
            engine: replayed.engine,
            log: LogWriter::open(cfg.log_path)?,
            key,
            checkpoint_every: cfg.checkpoint_every.max(1),
            chain_id: cfg.chain_id,
            vault: cfg.vault,
            stream,
            withdrawals,
            outbox: Vec::new(),
        })
    }

    /// Whether the log was empty, so genesis has not been sequenced.
    pub fn is_fresh(&self) -> bool {
        self.next_seq == 0
    }

    /// Sequences the genesis config as ordinary admin commands, then checkpoints.
    /// Error cases:
    /// - the engine rejects any genesis command
    /// - the log cannot be written
    pub fn genesis(&mut self, genesis: &Genesis) -> anyhow::Result<()> {
        let mut cmds = vec![Command::Admin(AdminCommand::SetParams(genesis.params.clone()))];
        cmds.extend(genesis.benchmarks.iter().map(|b| {
            Command::Admin(AdminCommand::SetBenchmarkRate { benchmark: b.name.clone(), annual_bps: b.annual_bps })
        }));
        cmds.extend(genesis.markets.iter().map(|m| Command::Admin(AdminCommand::UpsertMarket(m.clone()))));
        cmds.push(Command::HouseCapital { amount: genesis.house_capital, chain_ref: "genesis".into() });
        for cmd in cmds {
            let events = self.sequence(cmd)?;
            if let Some(Event::AdminRejected { reason }) =
                events.iter().find(|e| matches!(e, Event::AdminRejected { .. }))
            {
                bail!("genesis rejected: {reason}");
            }
        }
        self.checkpoint()?;
        self.flush()
    }

    /// Sequences requests until every sender is gone, then writes a final checkpoint. Each batch is
    /// flushed before its replies, stream messages, and queries go out.
    /// Error cases:
    /// - the log cannot be written; the node must stop rather than acknowledge undurable commands
    pub async fn run(mut self, mut rx: mpsc::Receiver<Request>) -> anyhow::Result<()> {
        let mut replies = Vec::new();
        let mut queries: Vec<Query> = Vec::new();
        while let Some(first) = rx.recv().await {
            let mut next = Some(first);
            while let Some(req) = next.take() {
                match req {
                    Request::Submit { cmd, reply } => {
                        let events = self.sequence(cmd)?;
                        if let Some(reply) = reply {
                            replies.push((reply, events));
                        }
                    }
                    Request::Query(q) => queries.push(q),
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

    /// Stamps, logs, and applies one command, signs any approved withdrawal, and checkpoints on
    /// schedule. The command's span carries its seq as `id`.
    /// Error cases:
    /// - the log cannot be written
    /// - the engine refuses the sequence, which means this sequencer is broken
    fn sequence(&mut self, cmd: Command) -> anyhow::Result<Vec<Event>> {
        // The host clock is untrusted inside a TEE, but it can't move sequenced time backwards.
        // TODO: bound drift against a trusted time source.
        let ts = now_ms().max(self.last_ts);
        let sequenced = Sequenced { seq: self.next_seq, ts, cmd };
        let _span = info_span!("command", id = sequenced.seq).entered();
        self.log.append(&LogEntry::Command(sequenced.clone()))?;
        let events = self.engine.apply(&sequenced).context("sequencer produced an invalid sequence")?;
        self.next_seq += 1;
        self.last_ts = ts;

        for event in &events {
            alert_on(event);
            if let Event::WithdrawalApproved { account, amount, nonce } = event {
                self.sign_withdrawal(*account, *amount, *nonce);
            }
        }
        self.outbox.push(Arc::new(StreamMsg::Command { sequenced, events: events.clone() }));
        if self.next_seq.is_multiple_of(self.checkpoint_every) {
            self.checkpoint()?;
        }
        Ok(events)
    }

    /// Signs an approved withdrawal and hands it to custody and the stream.
    fn sign_withdrawal(&mut self, account: AccountId, amount: Fixed, nonce: u64) {
        let signature = self.key.sign_withdrawal(self.chain_id, &self.vault.0, account, amount, nonce);
        let withdrawal =
            SignedWithdrawal { account, amount, nonce, chain_id: self.chain_id, vault: self.vault, signature };
        info!(%account, nonce, "withdrawal signed");
        if self.withdrawals.send(withdrawal.clone()).is_err() {
            error!(alert = "custody_unavailable", %account, nonce, "signed withdrawal has no custody consumer");
        }
        self.outbox.push(Arc::new(StreamMsg::Withdrawal(withdrawal)));
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
        self.log.append(&LogEntry::Checkpoint(cp.clone()))?;
        self.outbox.push(Arc::new(StreamMsg::Checkpoint(cp)));
        self.last_checkpoint = Some(seq);
        Ok(())
    }

    /// Makes buffered log entries durable, then publishes what they produced.
    /// Error cases:
    /// - the log cannot be written
    fn flush(&mut self) -> anyhow::Result<()> {
        self.log.flush()?;
        for msg in self.outbox.drain(..) {
            let _ = self.stream.send(msg);
        }
        Ok(())
    }
}

/// Re-applies a log and checks every checkpoint root against the state the open-source engine
/// produces. The same code serves crash recovery and third-party audit (`engine replay`).
/// Error cases:
/// - a command out of sequence or with time running backwards
/// - a checkpoint for a seq other than the last applied
/// - a checkpoint root that differs from the replayed state
pub fn replay(entries: &[LogEntry]) -> anyhow::Result<Replayed> {
    let mut engine = Engine::new();
    let (mut commands, mut checkpoints) = (0, 0);
    for entry in entries {
        match entry {
            LogEntry::Command(s) => {
                engine.apply(s).with_context(|| format!("applying seq {}", s.seq))?;
                commands += 1;
            }
            LogEntry::Checkpoint(cp) => {
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
    Ok(Replayed { engine, commands, checkpoints })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tick(seq: u64) -> LogEntry {
        LogEntry::Command(Sequenced { seq, ts: 0, cmd: Command::Tick })
    }

    fn checkpoint(seq: u64, root: String) -> LogEntry {
        LogEntry::Checkpoint(Checkpoint { seq, root, signature: String::new(), signer: String::new() })
    }

    #[test]
    fn replay_verifies_checkpoint_roots() {
        let mut engine = Engine::new();
        engine.apply(&Sequenced { seq: 0, ts: 0, cmd: Command::Tick }).unwrap();
        let root = format!("0x{}", hex::encode(engine.state_root()));
        let replayed = replay(&[tick(0), checkpoint(0, root)]).unwrap();
        assert_eq!((replayed.commands, replayed.checkpoints), (1, 1));
    }

    #[test]
    fn replay_refuses_a_tampered_root_or_a_misplaced_checkpoint() {
        let err = replay(&[tick(0), checkpoint(0, "0xbad".into())]).err().unwrap();
        assert!(format!("{err:#}").contains("state root mismatch"), "{err:#}");
        let err = replay(&[tick(0), checkpoint(5, "0x".into())]).err().unwrap();
        assert!(format!("{err:#}").contains("checkpoint for seq 5"), "{err:#}");
    }

    #[test]
    fn replay_refuses_a_gap_in_the_log() {
        let err = replay(&[tick(0), tick(2)]).err().unwrap();
        assert!(format!("{err:#}").contains("applying seq 2"), "{err:#}");
    }
}
