//! Account history: every event that names an account, indexed from the log the sequencer
//! publishes and served newest first. The index is derived from the log alone, never from engine
//! state, so removing its file rebuilds it from the log's segments. On a restart it resumes after the
//! last command it took, once that command is found unchanged in the log. History is public, like
//! the log it comes from.
//!
//! TODO: the index sits on host disk, which the host can rewrite between runs and during one. Each
//! event served names its place in the log, where anyone can check it; keeping the index in enclave
//! memory or on an integrity-protected volume would bound it.

mod store;
pub use store::Reader;

use std::path::Path;

use anyhow::{Context, bail, ensure};
use engine_core::event::Event;
use engine_core::types::{AccountId, Millis};
use serde::Serialize;
use tokio::sync::{mpsc, watch};
use tokio::task::spawn_blocking;
use tracing::info;

use crate::api::Feed;
use crate::config::HistorySection;
use crate::history::store::Writer;
use crate::log::{CommandEntry, LogEntry, LogReader, Manifest, Offset};
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
    /// The first command the index has not taken.
    resume_at: Offset,
}

impl History {
    /// Opens the index in `cfg.dir`, creating it if missing. An index that has taken commands must
    /// find the last of them unchanged in the log in `log_dir`: otherwise it was built from another
    /// log, or the log has lost commands since, and serving it would mix the two.
    /// Error cases:
    /// - the retention is invalid, or the index cannot be opened or read (see `Writer::open`)
    /// - the log cannot be read, ends before the last command the index took, or holds another
    ///   command at its seq
    pub fn open(cfg: &HistorySection, log_dir: &Path, manifest: watch::Receiver<Manifest>) -> anyhow::Result<Self> {
        let path = cfg.dir.join(INDEX_FILE);
        let writer = Writer::open(&path, cfg.retention_ms().context("history.retention")?)?;
        let resume_at = match writer.last_command()? {
            None => Offset::start_of(0),
            Some(last) => {
                check_in_log(&last, log_dir, manifest).with_context(|| {
                    format!(
                        "the history index in {} does not match the log; once the log is known to be right, remove the directory to rebuild the index",
                        cfg.dir.display()
                    )
                })?;
                Offset::start_of(last.sequenced.seq + 1)
            }
        };
        info!(dir = %cfg.dir.display(), from = %resume_at, "history index opened");
        Ok(Self { writer, reader: Reader::new(path), resume_at })
    }

    /// Where the index resumes: the first command it has not taken.
    pub fn resume_at(&self) -> Offset {
        self.resume_at
    }

    /// A reader of this index.
    pub fn reader(&self) -> Reader {
        self.reader.clone()
    }

    /// Takes every command `feed` yields into the index until the sequencer stops. A blocking thread
    /// writes, so neither a slow disk nor a long catch-up reaches the sequencer: whatever the live
    /// channel drops meanwhile, the feed reads from the log on disk.
    /// Error cases:
    /// - the feed fails (see `Feed::next`)
    /// - the writer fails (see `Writer::write`) or panics
    pub async fn run(self, mut feed: Feed) -> anyhow::Result<()> {
        let (tx, rx) = mpsc::channel(QUEUE);
        let writer = self.writer;
        let mut written = spawn_blocking(move || writer.run(rx));
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

/// Checks that the log in `log_dir` holds `last` at its seq.
/// Error cases:
/// - the log cannot be read (see `LogReader`)
/// - it ends before `last`'s seq, or holds another command there
fn check_in_log(last: &CommandEntry, log_dir: &Path, manifest: watch::Receiver<Manifest>) -> anyhow::Result<()> {
    let at = Offset::start_of(last.sequenced.seq);
    match LogReader::open(log_dir, at, manifest)?.next_entry()? {
        Some((offset, LogEntry::Command(logged))) if offset == at => {
            ensure!(logged == *last, "seq {} in the log is not the command the index took there", at.seq);
            Ok(())
        }
        Some((offset, _)) => bail!("the log holds {offset} where the command of seq {} should be", at.seq),
        None => bail!("the log ends before seq {}, the last command the index took", at.seq),
    }
}

#[cfg(test)]
mod tests;
