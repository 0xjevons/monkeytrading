//! Account history: every event that names an account, indexed from the log the sequencer
//! publishes and served newest first. The index is derived from the log alone, never from engine
//! state, so removing its file rebuilds it from the log's segments. On a restart it resumes after the
//! last command it took, once that command is found unchanged in the log, and serves pages only once
//! it has taken every command the log held at startup. History is public, like the log it comes
//! from.
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
    /// Set once the index has taken every command the log held at startup; readers wait for it.
    ready: watch::Sender<bool>,
    /// The last command the log held at startup, while the index has yet to take it.
    ready_at: Option<u64>,
    /// The first command the index has not taken.
    resume_at: Offset,
}

impl History {
    /// Opens the index in `cfg.dir`, creating it if missing. An index that has taken commands must
    /// find the last of them unchanged in the log in `log_dir`: otherwise it was built from another
    /// log, or the log has lost commands since, and serving it would mix the two. Call it before
    /// recovery writes to the log, so a refusal leaves the log as it was found.
    /// Error cases:
    /// - the retention is invalid, or the index cannot be opened or read (see `Writer::open`)
    /// - the log or its manifest cannot be read, or the log does not hold the last command the
    ///   index took, unchanged, at its seq
    pub fn open(cfg: &HistorySection, log_dir: &Path) -> anyhow::Result<Self> {
        let path = cfg.dir.join(INDEX_FILE);
        let writer = Writer::open(&path, cfg.retention_ms().context("history.retention")?)?;
        let resume_at = match writer.last_command()? {
            None => Offset::start_of(0),
            Some(last) => {
                check_in_log(&last, log_dir).with_context(|| {
                    format!(
                        "the history index in {} does not match the log; once the log is known to be right, remove the directory to rebuild the index",
                        cfg.dir.display()
                    )
                })?;
                Offset::start_of(last.sequenced.seq + 1)
            }
        };
        info!(dir = %cfg.dir.display(), from = %resume_at, "history index opened");
        let (ready, ready_rx) = watch::channel(false);
        Ok(Self { writer, reader: Reader::new(path, ready_rx), ready, ready_at: None, resume_at })
    }

    /// Records `tip`, the last command in the log once recovery has written to it: readers are let
    /// in once the index has taken it, or now when it already has.
    pub fn catch_up_to(&mut self, tip: Option<u64>) {
        self.ready_at = tip.filter(|tip| *tip >= self.resume_at.seq);
        if self.ready_at.is_none() {
            self.ready.send_replace(true);
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

/// Checks that the log in `log_dir` holds `last` at its seq, reading completed segments against the
/// manifest on disk, which recovery checks the same way.
/// Error cases:
/// - the manifest or the log cannot be read (see `LogReader`)
/// - the log ends before `last`'s seq, or holds another command there
fn check_in_log(last: &CommandEntry, log_dir: &Path) -> anyhow::Result<()> {
    let at = Offset::start_of(last.sequenced.seq);
    let manifest = watch::channel(Manifest::load(log_dir)?).1;
    let logged = LogReader::open(log_dir, at, manifest)?.next_entry()?;
    ensure!(
        logged == Some((at, LogEntry::Command(last.clone()))),
        "the log ends before seq {seq}, or holds another command there than the one the index took",
        seq = at.seq
    );
    Ok(())
}

#[cfg(test)]
mod tests;
