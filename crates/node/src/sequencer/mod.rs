//! The sequencer owns the engine. Commands arrive over one channel, get a seq and a timestamp, and
//! are applied in order; each then goes into the log with the events it produced. Nothing is
//! acknowledged or published until its log entry is flushed. One task, no locks, and no queue
//! beyond the channel: a fill costs about one engine step plus a buffered write.

mod replay;
pub use replay::{Replayed, replay, replay_path, replay_path_in_domain, replay_path_with_trust, replay_with_signer};
mod checkpoints;
use checkpoints::Gate;
mod messages;
pub use messages::{Query, Request, SignedWithdrawal, StreamBody, StreamMsg};

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use engine_core::auth::{ClientAuth, Operator};
use engine_core::command::{Command, Sequenced};
use engine_core::event::Event;
use engine_core::types::{AccountId, Millis};
use engine_core::{Engine, Fixed};
use tokio::sync::{broadcast, mpsc, watch};
use tracing::{error, info, info_span, warn};

use crate::clock::Clock;
use crate::log::{CommandEntry, LogEntry, LogWriter, Manifest, SegmentedLog};
use crate::metrics::Metrics;
use crate::sequencer::alerts::{DeficitAlert, alert_on};
use crate::sequencer::replay::Replay;
use crate::tee::EnclaveKey;
use crate::trusted_time::{Sample, TimeGate};

/// Most requests of any kind taken into one batch before it is flushed and answered.
const MAX_BATCH: usize = 1024;

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
    clock: Arc<dyn Clock>,
    time: TimeGate,
    /// Commands the engine must accept, waiting for a stamp trusted time admits, each group with
    /// what it is for.
    required: Vec<(&'static str, Vec<Command>)>,
    client_auth: ClientAuth,
    /// The alert for house NAV below zero.
    deficit: DeficitAlert,
    operator: Operator,
    metrics: Arc<Metrics>,
    gate: Option<Gate>,
    unpaid: Fixed,
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
    /// The operator the config gives, which a log that records one must match.
    pub operator: Operator,
    /// Where the sequencer records its metrics.
    pub metrics: Arc<Metrics>,
    pub(crate) checkpoint_parent: crate::log::CheckpointParent,
    pub(crate) checkpoints: Option<mpsc::Sender<crate::checkpoints::Work>>,
}

/// A log read from disk and replayed, before anything is written to it.
pub struct ReplayedLog {
    log: SegmentedLog,
    replayed: Replayed,
}

impl Sequencer {
    /// Rebuilds state by replaying the existing log, if there is one, and opens it for appending.
    /// Signs, logs, and hands to custody any withdrawal the log ends before signing.
    ///
    /// The node checks the posted chain anchor before invoking recovery.
    /// Error cases:
    /// - the log cannot be read or replayed (see `read_log`)
    /// - the log records a client auth or an operator other than the config's, or a genesis it did
    ///   not apply in full
    /// - the log cannot be opened for appending, or written
    pub fn recover(
        cfg: SequencerConfig<'_>,
        key: Arc<EnclaveKey>,
        stream: broadcast::Sender<Arc<StreamMsg>>,
        withdrawals: mpsc::UnboundedSender<SignedWithdrawal>,
    ) -> anyhow::Result<Self> {
        let log = Self::read_log_with_trust(
            cfg.log_dir,
            Some(crate::log::CheckpointDomain { chain_id: cfg.chain_id, vault: cfg.vault }),
            Some(AccountId(key.address())),
            |_| {},
        )?;
        Self::recover_from(log, cfg, key, stream, withdrawals)
    }

    /// Reads the log in `dir` a segment at a time and replays it as it goes, handing each entry to
    /// `observe` as well, so that a check of the log before recovery sees exactly what it replays.
    /// Error cases:
    /// - the log cannot be read, or fails its manifest check (see `SegmentedLog::open`) or replay
    ///   (see `Replay`)
    pub fn read_log(dir: &Path, mut observe: impl FnMut(&LogEntry)) -> anyhow::Result<ReplayedLog> {
        Self::read_log_in_domain(dir, None, &mut observe)
    }

    /// Reads one log in a known custody deployment; signatures and the observer see that read.
    /// Error cases:
    /// - log reading or strict signature/root replay fails
    pub fn read_log_in_domain(
        dir: &Path,
        domain: Option<crate::log::CheckpointDomain>,
        mut observe: impl FnMut(&LogEntry),
    ) -> anyhow::Result<ReplayedLog> {
        Self::read_log_with_trust(dir, domain, None, &mut observe)
    }

    /// Reads one authenticated stream with an independently trusted withdrawal authority.
    /// Error cases:
    /// - reading, replay, or release authorization fails
    pub fn read_log_with_trust(
        dir: &Path,
        domain: Option<crate::log::CheckpointDomain>,
        signer: Option<AccountId>,
        mut observe: impl FnMut(&LogEntry),
    ) -> anyhow::Result<ReplayedLog> {
        let mut replay = Replay::with_trust(domain, signer);
        let log = SegmentedLog::open(dir, |entry| {
            observe(&entry);
            replay.apply(&entry)
        })?;
        Ok(ReplayedLog { log, replayed: replay.finish() })
    }

    /// Recovers the log `read_log` already authenticated.
    /// Error cases:
    /// - the log records a client auth or an operator other than the config's, or a genesis it did
    ///   not apply in full (see `GenesisProgress::ensure_closed`)
    /// - the log cannot be opened for appending, or written
    pub fn recover_from(
        log: ReplayedLog,
        cfg: SequencerConfig<'_>,
        key: Arc<EnclaveKey>,
        stream: broadcast::Sender<Arc<StreamMsg>>,
        withdrawals: mpsc::UnboundedSender<SignedWithdrawal>,
    ) -> anyhow::Result<Self> {
        let ReplayedLog { log, replayed } = log;
        let Replayed {
            engine,
            commands,
            checkpoints,
            unsigned,
            deferred,
            unpaid,
            genesis,
            last_checkpoint_ts,
            last_signed_checkpoint,
            ..
        } = replayed;
        // A config naming another client auth or operator, and so another chain, vault, or admin key,
        // is the wrong one for this log: refuse before anything is written or signed under it.
        if let Some(recorded) = engine.client_auth().filter(|recorded| **recorded != cfg.client_auth) {
            bail!("the log records client auth {recorded:?}, but the config gives {:?}", cfg.client_auth);
        }
        if let Some(recorded) = engine.operator().filter(|recorded| **recorded != cfg.operator) {
            bail!("the log records operator {recorded:?}, but the config gives {:?}", cfg.operator);
        }
        genesis.ensure_closed()?;
        if !deferred.is_empty() && cfg.checkpoints.is_none() {
            bail!("checkpoint-gated withdrawals require the checkpoint service");
        }
        let checkpoint_unsigned = cfg.checkpoints.is_some() && (!unsigned.is_empty() || !deferred.is_empty());
        info!(commands, checkpoints, "recovered from log");
        if let Some(ts) = last_checkpoint_ts {
            cfg.metrics.last_checkpoint_seconds.set(ts as f64 / 1000.0);
        }
        let mut sequencer = Self {
            next_seq: engine.last_seq().map_or(0, |s| s + 1),
            last_ts: engine.now(),
            last_checkpoint: engine.last_seq(),
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
            deficit: DeficitAlert::default(),
            operator: cfg.operator,
            metrics: cfg.metrics,
            gate: cfg.checkpoints.map(|sink| Gate::new(sink, cfg.checkpoint_parent)),
            unpaid,
        };
        {
            let _span = info_span!("command", id = sequencer.engine.last_seq()).entered();
            sequencer.deficit.note(sequencer.engine.house_nav());
        }
        for (account, amount, nonce) in unsigned {
            let _span = info_span!("command", id = sequencer.engine.last_seq()).entered();
            warn!(%account, nonce, "the log ends before this approved withdrawal was signed");
            if let Some(gate) = &mut sequencer.gate {
                gate.pending.push((sequencer.engine.last_seq().expect("approval has a seq"), account, amount, nonce));
            } else {
                sequencer.sign_withdrawal(account, amount, nonce)?;
            }
        }
        if let Some(gate) = &mut sequencer.gate {
            gate.pending.extend(deferred);
        }
        if checkpoint_unsigned {
            if sequencer
                .gate
                .as_ref()
                .is_some_and(|gate| gate.parent.has_checkpoint && Some(gate.parent.seq) == sequencer.engine.last_seq())
            {
                let cp = last_signed_checkpoint.context("finalized recovery parent lacks its signed checkpoint")?;
                sequencer.queue_checkpoint(cp)?;
            } else {
                sequencer.checkpoint()?;
            }
        }
        sequencer.flush()?;
        Ok(sequencer)
    }

    /// Sequences requests until every sender is gone, then writes a final checkpoint. Each batch,
    /// of at most `MAX_BATCH` requests, is flushed before its replies, stream messages, and queries
    /// go out, and synced to storage first when it carries a sync request. A command trusted time
    /// refuses is dropped with its reply, which the API answers as unavailable.
    /// Error cases:
    /// - the log cannot be written or synced; the node must stop rather than acknowledge
    ///   undurable commands
    /// - the engine rejects a queued required command
    pub async fn run(mut self, mut rx: mpsc::Receiver<Request>) -> anyhow::Result<()> {
        let mut replies = Vec::new();
        let mut queries: Vec<Query> = Vec::new();
        let mut syncs = Vec::new();
        while let Some(first) = rx.recv().await {
            let started = self.clock.instant();
            let mut sequenced = false;
            let (mut next, mut taken) = (Some(first), 0);
            while let Some(req) = next.take() {
                taken += 1;
                match req {
                    Request::Submit { cmd, reply } => {
                        let events = self.sequence(cmd)?;
                        sequenced |= events.is_some();
                        if let (Some(events), Some(reply)) = (events, reply) {
                            replies.push((reply, events));
                        }
                    }
                    Request::Query(q) => queries.push(q),
                    Request::TimeSample(sample) => self.take_sample(sample)?,
                    Request::Sync(reply) => syncs.push(reply),
                    Request::CheckpointStatus { posted, healthy } => self.checkpoint_status(posted, healthy)?,
                }
                if taken < MAX_BATCH {
                    next = rx.try_recv().ok();
                }
            }
            self.flush()?;
            if !syncs.is_empty() {
                self.log.sync()?;
                let last_seq = self.engine.last_seq();
                syncs.drain(..).for_each(|reply| {
                    let _ = reply.send(last_seq);
                });
            }
            replies.drain(..).for_each(|(reply, events)| {
                let _ = reply.send(events);
            });
            // Only admitted commands are written: a batch of reads or refusals would dilute the latency.
            if sequenced {
                self.metrics.batch_seconds.observe(self.clock.instant().duration_since(started).as_secs_f64());
            }
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
        if matches!(cmd, Command::Withdraw { .. }) && self.gate.as_ref().is_some_and(|gate| !gate.healthy) {
            warn!(alert = "withdrawals_paused", "withdrawal refused until custody reconciles");
            return Ok(None);
        }
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
        self.metrics.commands.inc();

        for event in &events {
            alert_on(event);
            self.metrics.record(event);
            if let Event::WithdrawalApproved { account, amount, nonce } = event {
                self.unpaid += *amount;
                if let Some(gate) = &mut self.gate {
                    gate.pending.push((
                        self.engine.last_seq().expect("applied command has a seq"),
                        *account,
                        *amount,
                        *nonce,
                    ));
                } else {
                    self.sign_withdrawal(*account, *amount, *nonce)?;
                }
            }
        }
        self.deficit.note(self.engine.house_nav());
        if self.next_seq.is_multiple_of(self.checkpoint_every)
            || self.gate.as_ref().is_some_and(|gate| {
                gate.pending.last().is_some_and(|w| w.0 == self.engine.last_seq().expect("applied command has a seq"))
            })
        {
            self.checkpoint()?;
        }
        Ok(events)
    }

    /// Appends an entry to the log and queues it for the stream at its offset.
    /// Error cases:
    /// - the log cannot be written
    fn record(&mut self, entry: LogEntry) -> anyhow::Result<()> {
        let offset = self.log.append(&entry)?;
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
            approved_seq: None,
        }))
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

mod alerts;
pub(crate) mod genesis;
pub use genesis::{GenesisProgress, genesis_commands};
mod required;

#[cfg(test)]
mod batch_tests;
#[cfg(test)]
mod margin_tests;
#[cfg(test)]
mod metrics_tests;
#[cfg(test)]
mod tests;

#[cfg(test)]
mod checkpoint_tests;
#[cfg(test)]
pub(crate) mod fixture_tests;
