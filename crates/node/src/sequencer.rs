//! The sequencer owns the engine. Commands arrive over one channel, get a
//! seq and a timestamp, go into the log, and are applied in order. Nothing
//! is acknowledged or published until its log entry is flushed. One thread,
//! no locks, and no queue beyond the channel: a fill costs about one engine
//! step plus a buffered write.

use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use engine_core::command::{AdminCommand, Command, Sequenced};
use engine_core::event::Event;
use engine_core::types::{AccountId, Millis};
use engine_core::{Engine, Fixed};
use serde::Serialize;
use tokio::sync::{broadcast, mpsc, oneshot};

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

impl Sequencer {
    /// Rebuilds state by replaying the existing log, if there is one.
    ///
    /// TODO: the log sits on host storage, so the host could truncate it.
    /// Before serving, check the replayed root against the latest root
    /// posted on-chain.
    pub fn recover(
        cfg: SequencerConfig<'_>,
        key: Arc<EnclaveKey>,
        stream: broadcast::Sender<Arc<StreamMsg>>,
        withdrawals: mpsc::UnboundedSender<SignedWithdrawal>,
    ) -> anyhow::Result<Self> {
        let replayed = replay(&log::read_all(cfg.log_path)?)?;
        tracing::info!(commands = replayed.commands, checkpoints = replayed.checkpoints, "recovered from log");
        Ok(Self {
            next_seq: replayed.engine.last_seq().map_or(0, |s| s + 1),
            last_ts: replayed.engine.now(),
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

    pub fn is_fresh(&self) -> bool {
        self.next_seq == 0
    }

    /// Sequences the genesis config as ordinary admin commands.
    pub fn genesis(&mut self, g: &Genesis) -> anyhow::Result<()> {
        let mut cmds = vec![Command::Admin(AdminCommand::SetParams(g.params.clone()))];
        cmds.extend(g.benchmarks.iter().map(|b| {
            Command::Admin(AdminCommand::SetBenchmarkRate { benchmark: b.name.clone(), annual_bps: b.annual_bps })
        }));
        cmds.extend(g.markets.iter().map(|m| Command::Admin(AdminCommand::UpsertMarket(m.clone()))));
        cmds.push(Command::HouseCapital { amount: g.house_capital, chain_ref: "genesis".into() });
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
            for (reply, events) in replies.drain(..) {
                let _ = reply.send(events);
            }
            for q in queries.drain(..) {
                q(&self.engine);
            }
        }
        Ok(())
    }

    fn sequence(&mut self, cmd: Command) -> anyhow::Result<Vec<Event>> {
        // The host clock is untrusted inside a TEE, but it can't move
        // sequenced time backwards. TODO: bound drift against a trusted
        // time source.
        let ts = now_ms().max(self.last_ts);
        let sequenced = Sequenced { seq: self.next_seq, ts, cmd };
        self.log.append(&LogEntry::Command(sequenced.clone()))?;
        let events = self.engine.apply(&sequenced).context("sequencer produced an invalid sequence")?;
        self.next_seq += 1;
        self.last_ts = ts;

        for e in &events {
            if let Event::WithdrawalApproved { account, amount, nonce } = e {
                let signature = self.key.sign_withdrawal(self.chain_id, &self.vault.0, *account, *amount, *nonce);
                let w = SignedWithdrawal {
                    account: *account,
                    amount: *amount,
                    nonce: *nonce,
                    chain_id: self.chain_id,
                    vault: self.vault,
                    signature,
                };
                let _ = self.withdrawals.send(w.clone());
                self.outbox.push(Arc::new(StreamMsg::Withdrawal(w)));
            }
        }
        self.outbox.push(Arc::new(StreamMsg::Command { sequenced, events: events.clone() }));
        if self.next_seq.is_multiple_of(self.checkpoint_every) {
            self.checkpoint()?;
        }
        Ok(events)
    }

    /// TODO: hashing the whole state is O(state). Replace it with an
    /// incremental Merkle root before accounts number in the millions.
    fn checkpoint(&mut self) -> anyhow::Result<()> {
        let seq = self.next_seq - 1;
        let root = self.engine.state_root();
        let cp = Checkpoint {
            seq,
            root: format!("0x{}", hex::encode(root)),
            signature: self.key.sign_checkpoint(seq, &root),
            signer: self.key.address_hex(),
        };
        self.log.append(&LogEntry::Checkpoint(cp.clone()))?;
        self.outbox.push(Arc::new(StreamMsg::Checkpoint(cp)));
        Ok(())
    }

    fn flush(&mut self) -> anyhow::Result<()> {
        self.log.flush()?;
        for msg in self.outbox.drain(..) {
            let _ = self.stream.send(msg);
        }
        Ok(())
    }
}

pub struct Replayed {
    pub engine: Engine,
    pub commands: u64,
    pub checkpoints: u64,
}

/// Re-applies a log and checks every checkpoint root against the state the
/// open-source engine actually produces. The same code serves crash
/// recovery and third-party audit (`engine replay`).
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

fn now_ms() -> Millis {
    SystemTime::now().duration_since(UNIX_EPOCH).expect("clock after epoch").as_millis() as Millis
}
