//! Account history: every event that names an account, indexed from the log the sequencer
//! publishes and served newest first. The index is derived from the log alone, never from engine
//! state, so it can always be rebuilt from the log's segments. On a restart it resumes after the
//! last command it took, once that command is found unchanged in the log; an index the log does not
//! match, one pruned under another retention, or a damaged file is discarded and rebuilt instead. It
//! serves pages only once it has taken every command the log held at startup. History is public,
//! like the log it comes from.
//!
//! TODO: the index sits on host disk, which the host can rewrite between runs and during one. SQLite
//! reads it in defensive mode and refuses a schema this code did not create, which bounds what a
//! rewritten file can make it run, but the rows themselves are unchecked. The startup check compares
//! one command, the last the index has taken, so it rebuilds after a log rolled back, truncated, or
//! replaced by another, but not after one forged to end with that same command; a host able to forge
//! the log can rewrite the index anyway. Each event served names its place in the log, where anyone
//! can check it; keeping the index in enclave memory or on an integrity-protected volume would bound
//! both.

mod store;
pub use store::Reader;

use std::path::Path;

use anyhow::{Context, bail, ensure};
use engine_core::event::Event;
use engine_core::types::{AccountId, Millis};
use serde::Serialize;
use tokio::sync::{mpsc, watch};
use tokio::task::spawn_blocking;
use tracing::{info, warn};

use crate::api::Feed;
use crate::config::HistorySection;
use crate::history::store::{Writer, discard};
use crate::log::{CommandEntry, LogEntry, Offset};
use crate::sequencer::StreamBody;

/// The `type` of every event that names an account: what the `kind` filter accepts.
pub const KINDS: [&str; 11] = [
    "deposited",
    "deposit_rejected",
    "withdrawal_approved",
    "withdrawal_rejected",
    "order_accepted",
    "order_rejected",
    "order_cancelled",
    "fill",
    "close_out",
    "bad_debt",
    "financing",
];
/// The SQLite file of the index, in `history.dir`; SQLite keeps its `-wal` and `-shm` files beside it.
pub const INDEX_FILE: &str = "history.sqlite";
/// Commands queued between the log feed and the index writer.
const QUEUE: usize = 1024;

/// One page's account, filter, exclusive bounds, and size.
pub struct PageQuery {
    pub account: AccountId,
    pub kind: Option<String>,
    pub before: Option<Offset>,
    pub after: Option<Offset>,
    pub limit: usize,
}

/// A page of an account's events, newest first.
#[derive(Debug, PartialEq, Serialize)]
pub struct Page {
    pub events: Vec<HistoryEvent>,
    /// The `before` for the next, older page; `None` when no older event matches.
    pub next: Option<Offset>,
}

/// An account event and where it sits in the log.
#[derive(Debug, PartialEq, Serialize)]
pub struct HistoryEvent {
    /// The seq of the command that produced the event, and the event's index among that command's
    /// events, written `<seq>:<index>`.
    pub cursor: Offset,
    /// The command's sequenced time.
    pub ts: Millis,
    #[serde(flatten)]
    pub event: Event,
}

/// The index, checked against the log and ready to catch up.
pub struct History {
    writer: Writer,
    reader: Reader,
    /// Set once the index has taken every command the log held at startup; readers wait for it.
    ready: watch::Sender<bool>,
    /// The last command the log held at startup, while the index has yet to take it.
    ready_at: Option<u64>,
    /// The first command the index has not taken.
    resume_at: Offset,
}

impl History {
    /// Opens the index in `cfg.dir`, creating it if missing, and checks it against `entries`, the log
    /// recovery replays next, so that it resumes after the last command it took. An index whose
    /// file is damaged or holds another schema, whose rows were pruned under another retention than
    /// `cfg` gives, or whose last command `entries` do not hold unchanged is discarded with a
    /// Warning alert and rebuilt from the start of the log. A power loss can leave the index ahead
    /// of the log, and a log rolled back, truncated, or replaced does the same; a log forged to end
    /// with that same command passes (see the module's TODO).
    /// Error cases:
    /// - the retention is invalid
    /// - no index was on disk and a new one cannot be opened, or a discarded one cannot be removed or
    ///   replaced (see `Writer::open`)
    pub fn open(cfg: &HistorySection, entries: &[LogEntry]) -> anyhow::Result<Self> {
        let path = cfg.dir.join(INDEX_FILE);
        let retention_ms = cfg.retention_ms().context("history.retention")?;
        let found = path.exists();
        let (writer, resume_at) = match resume(&path, retention_ms, entries) {
            Ok(resumed) => resumed,
            Err(reason) if found => {
                warn!(
                    alert = "history_index_discarded",
                    dir = %cfg.dir.display(),
                    "rebuilding the history index from the log: {reason:#}"
                );
                discard(&path)?;
                (Writer::open(&path, retention_ms)?.0, Offset::start_of(0))
            }
            Err(e) => return Err(e),
        };
        info!(dir = %cfg.dir.display(), from = %resume_at, "history index opened");
        let (ready, ready_rx) = watch::channel(false);
        Ok(Self { writer, reader: Reader::new(path, ready_rx), ready, ready_at: None, resume_at })
    }

    /// Records `tip`, the last command in the log recovery replayed, the same read `open` checked:
    /// readers are let in once the index has taken it, or now when it already has.
    pub fn catch_up_to(&mut self, tip: Option<u64>) {
        self.ready_at = tip.filter(|tip| *tip >= self.resume_at.seq);
        if self.ready_at.is_none() {
            self.ready.send_replace(true);
            info!(?tip, "history index holds the log's tip");
        }
    }

    /// Where the index resumes: the first command it has not taken.
    pub fn resume_at(&self) -> Offset {
        self.resume_at
    }

    /// A reader of this index.
    pub fn reader(&self) -> Reader {
        self.reader.clone()
    }

    /// Takes every command `feed` yields into the index until the sequencer stops, letting readers
    /// in once it has taken the tip `catch_up_to` recorded. A blocking thread writes, so neither a
    /// slow disk nor a long catch-up reaches the sequencer: whatever the live channel drops
    /// meanwhile, the feed reads from the log on disk.
    /// Error cases:
    /// - the feed fails (see `Feed::next`)
    /// - the writer fails (see `Writer::write`) or panics
    pub async fn run(self, mut feed: Feed) -> anyhow::Result<()> {
        let (tx, rx) = mpsc::channel(QUEUE);
        let (writer, ready, ready_at) = (self.writer, self.ready, self.ready_at);
        let mut written = spawn_blocking(move || writer.run(rx, ready, ready_at));
        let forward = async move {
            while let Some(msg) = feed.next().await? {
                // A closed queue means the writer stopped; its result says why.
                if matches!(msg.body, StreamBody::Command(_)) && tx.send(msg).await.is_err() {
                    break;
                }
            }
            anyhow::Ok(())
        };
        tokio::select! {
            forwarded = forward => forwarded?,
            stopped = &mut written => {
                stopped.context("history writer panicked")??;
                bail!("history writer stopped");
            }
        }
        written.await.context("history writer panicked")?
    }
}

/// The account an event belongs to, or `None` for an event that names no account.
pub fn account_of(event: &Event) -> Option<AccountId> {
    match event {
        Event::Deposited { account, .. }
        | Event::DepositRejected { account, .. }
        | Event::WithdrawalApproved { account, .. }
        | Event::WithdrawalRejected { account, .. }
        | Event::OrderAccepted { account, .. }
        | Event::OrderRejected { account, .. }
        | Event::OrderCancelled { account, .. }
        | Event::CloseOut { account, .. }
        | Event::BadDebt { account, .. }
        | Event::Financing { account, .. } => Some(*account),
        Event::Fill(fill) => Some(fill.account),
        Event::HouseCapitalMoved { .. }
        | Event::MarketOpened { .. }
        | Event::MarketClosed { .. }
        | Event::StressChanged { .. }
        | Event::PricesRejected { .. }
        | Event::AdminApplied
        | Event::AdminRejected { .. } => None,
    }
}

/// Opens the index at `path` and returns where it resumes: after the last command it took, or at the
/// start of the log when it has taken none.
/// Error cases:
/// - the index cannot be opened, is damaged, or holds another schema (see `Writer::open`)
/// - its rows were pruned under another retention than `retention_ms`
/// - `entries`, the whole log, end before the last command it took, or hold another command at its
///   seq
fn resume(path: &Path, retention_ms: Millis, entries: &[LogEntry]) -> anyhow::Result<(Writer, Offset)> {
    let (writer, progress) = Writer::open(path, retention_ms)?;
    let Some(progress) = progress else {
        return Ok((writer, Offset::start_of(0)));
    };
    ensure!(
        progress.retention_ms == retention_ms,
        "its events were pruned under a retention of {} ms, not the {retention_ms} ms history.retention gives",
        progress.retention_ms
    );
    let seq = progress.command.sequenced.seq;
    let logged = logged_command(seq, entries)
        .with_context(|| format!("the log ends before seq {seq}, the last command the index took"))?;
    ensure!(*logged == progress.command, "the log holds another command at seq {seq}, the last command the index took");
    Ok((writer, Offset::start_of(seq + 1)))
}

/// The command `entries`, a whole log in order, hold at `seq`.
fn logged_command(seq: u64, entries: &[LogEntry]) -> Option<&CommandEntry> {
    let commands = entries.iter().rev().filter_map(|entry| match entry {
        LogEntry::Command(command) => Some(command),
        _ => None,
    });
    commands.take_while(|command| command.sequenced.seq >= seq).find(|command| command.sequenced.seq == seq)
}

#[cfg(test)]
mod tests;
