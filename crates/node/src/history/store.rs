//! The history index on disk: one SQLite file. `events` holds each account event under its place in
//! the log, `(seq, idx)`, so taking a command twice rewrites the same rows; `progress` holds the last
//! command taken, in the same transaction as its events.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use engine_core::types::Millis;
use rusqlite::{Connection, OpenFlags, OptionalExtension, ToSql, params};
use tokio::sync::{Semaphore, mpsc};
use tokio::task::spawn_blocking;
use tracing::debug;

use crate::history::{HistoryEvent, Page, PageQuery, account_of};
use crate::log::{CommandEntry, Offset};
use crate::sequencer::{StreamBody, StreamMsg};

/// The schema this code writes, kept in the file's `user_version`.
const SCHEMA_VERSION: i64 = 1;
const SCHEMA: &str = "
    CREATE TABLE events (
        seq INTEGER NOT NULL,
        idx INTEGER NOT NULL,
        account BLOB NOT NULL,
        kind TEXT NOT NULL,
        ts INTEGER NOT NULL,
        event TEXT NOT NULL,
        PRIMARY KEY (seq, idx)
    ) WITHOUT ROWID;
    CREATE INDEX events_by_account ON events (account, seq, idx);
    CREATE INDEX events_by_account_kind ON events (account, kind, seq, idx);
    CREATE INDEX events_by_ts ON events (ts);
    CREATE TABLE progress (
        id INTEGER PRIMARY KEY CHECK (id = 0),
        command TEXT NOT NULL
    );
";
/// A page, newest first. `INDEXED BY` makes SQLite refuse the query rather than plan a scan, so a
/// page reads at most `:limit` entries of the index.
const PAGE: &str = "SELECT seq, idx, ts, event FROM events INDEXED BY events_by_account
    WHERE account = :account AND (seq, idx) < (:before_seq, :before_idx) AND (seq, idx) > (:after_seq, :after_idx)
    ORDER BY seq DESC, idx DESC LIMIT :limit";
const PAGE_OF_KIND: &str = "SELECT seq, idx, ts, event FROM events INDEXED BY events_by_account_kind
    WHERE account = :account AND kind = :kind
        AND (seq, idx) < (:before_seq, :before_idx) AND (seq, idx) > (:after_seq, :after_idx)
    ORDER BY seq DESC, idx DESC LIMIT :limit";
/// Most commands written in one transaction.
const WRITE_BATCH: usize = 1024;
/// How long a connection waits on another's lock before it fails.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// Pages read at once, each on its own connection.
const MAX_READS: usize = 16;

/// Takes commands into the index: the one connection that writes to it.
pub(super) struct Writer {
    conn: Connection,
    retention_ms: Millis,
}

/// Reads pages of the index, each on a new read-only connection.
#[derive(Clone)]
pub struct Reader {
    path: PathBuf,
    permits: Arc<Semaphore>,
}

impl Writer {
    /// Opens the index at `path`, creating its directory, the file, and the schema if missing.
    /// Events leave the index `retention_ms` before the newest command it holds.
    /// Error cases:
    /// - the directory or the file cannot be created or opened, or the file is not a SQLite database
    /// - the file holds another version of the schema
    pub(super) fn open(path: &Path, retention_ms: Millis) -> anyhow::Result<Self> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let mut conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.busy_timeout(BUSY_TIMEOUT).context("setting the busy timeout")?;
        let mode: String = conn
            .pragma_update_and_check(None, "journal_mode", "wal", |row| row.get(0))
            .with_context(|| format!("opening {}", path.display()))?;
        ensure!(mode == "wal", "{} cannot use a write-ahead log, only {mode}", path.display());
        conn.pragma_update(None, "synchronous", "normal").context("setting synchronous")?;
        let tx = conn.transaction().context("starting the schema check")?;
        match tx.pragma_query_value(None, "user_version", |row| row.get(0)).context("reading the schema version")? {
            0 => {
                tx.execute_batch(SCHEMA).context("creating the history schema")?;
                tx.pragma_update(None, "user_version", SCHEMA_VERSION).context("setting the schema version")?;
            }
            SCHEMA_VERSION => {}
            other => bail!("{} holds history schema {other}, not {SCHEMA_VERSION}", path.display()),
        }
        tx.commit().context("committing the history schema")?;
        Ok(Self { conn, retention_ms })
    }

    /// The last command the index took, if any.
    /// Error cases:
    /// - the index cannot be read, or the command it holds does not parse
    pub(super) fn last_command(&self) -> anyhow::Result<Option<CommandEntry>> {
        let json: Option<String> = self
            .conn
            .query_row("SELECT command FROM progress WHERE id = 0", [], |row| row.get(0))
            .optional()
            .context("reading history progress")?;
        json.map(|json| serde_json::from_str(&json).context("history progress")).transpose()
    }

    /// Takes commands from `rx` until every sender is gone, writing whatever is queued in one
    /// transaction at a time.
    /// Error cases:
    /// - a write fails (see `write`)
    pub(super) fn run(mut self, mut rx: mpsc::Receiver<Arc<StreamMsg>>) -> anyhow::Result<()> {
        let mut batch = Vec::new();
        while let Some(msg) = rx.blocking_recv() {
            batch.push(msg);
            while batch.len() < WRITE_BATCH
                && let Ok(msg) = rx.try_recv()
            {
                batch.push(msg);
            }
            self.write(&batch)?;
            batch.clear();
        }
        Ok(())
    }

    /// In one transaction: stores the account events of the commands in `batch`, records the last
    /// command as taken, and drops events older than the retention allows.
    /// Error cases:
    /// - the transaction fails
    /// - a seq is beyond SQLite's integer range, or an event or command cannot be encoded
    pub(super) fn write(&mut self, batch: &[Arc<StreamMsg>]) -> anyhow::Result<()> {
        let commands: Vec<&CommandEntry> = batch
            .iter()
            .filter_map(|msg| match &msg.body {
                StreamBody::Command(command) => Some(command),
                _ => None,
            })
            .collect();
        let Some(last) = commands.last() else {
            return Ok(());
        };
        let tx = self.conn.transaction().context("starting a history write")?;
        {
            let mut insert = tx
                .prepare("INSERT OR REPLACE INTO events (seq, idx, account, kind, ts, event) VALUES (?1, ?2, ?3, ?4, ?5, ?6)")
                .context("preparing the history insert")?;
            for command in &commands {
                let (seq, ts) = (i64::try_from(command.sequenced.seq).context("seq")?, command.sequenced.ts);
                for (idx, event) in command.events.iter().flatten().enumerate() {
                    let Some(account) = account_of(event) else {
                        continue;
                    };
                    let json = serde_json::to_value(event).context("encoding an event")?;
                    let kind = json["type"].as_str().expect("events serialize with a type tag").to_owned();
                    let row = params![seq, idx as i64, &account.0[..], kind, ts, json.to_string()];
                    insert.execute(row).with_context(|| format!("storing event {seq}:{idx}"))?;
                }
            }
        }
        let taken = serde_json::to_string(last).context("encoding a command")?;
        tx.execute("INSERT OR REPLACE INTO progress (id, command) VALUES (0, ?1)", [taken])
            .context("storing history progress")?;
        let horizon = last.sequenced.ts.saturating_sub(self.retention_ms);
        tx.execute("DELETE FROM events WHERE ts < ?1", [horizon]).context("dropping events past retention")?;
        tx.commit().context("committing a history write")?;
        debug!(seq = last.sequenced.seq, commands = commands.len(), "history index took commands");
        Ok(())
    }
}

impl Reader {
    /// A reader of the index at `path`, which the writer creates.
    pub fn new(path: PathBuf) -> Self {
        Self { path, permits: Arc::new(Semaphore::new(MAX_READS)) }
    }

    /// One page of `query`, read on a blocking thread once one of `MAX_READS` permits is free.
    /// Error cases:
    /// - the index cannot be opened or read, or holds a row that does not parse
    pub async fn page(&self, query: PageQuery) -> anyhow::Result<Page> {
        let _permit = self.permits.acquire().await.expect("the read semaphore is never closed");
        let path = self.path.clone();
        spawn_blocking(move || read_page(&path, &query)).await.context("history reader task")?
    }
}

/// Reads one page of `query` from the index at `path`.
/// Error cases:
/// - the index cannot be opened or read, or holds a row that does not parse
fn read_page(path: &Path, query: &PageQuery) -> anyhow::Result<Page> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(path, flags).with_context(|| format!("opening {}", path.display()))?;
    conn.busy_timeout(BUSY_TIMEOUT).context("setting the busy timeout")?;
    let account = &query.account.0[..];
    let (before_seq, before_idx) = bound(query.before, i64::MAX);
    let (after_seq, after_idx) = bound(query.after, -1);
    // One more than the page, to learn whether an older page follows.
    let limit = i64::try_from(query.limit).context("page limit")?.saturating_add(1);
    let mut args: Vec<(&str, &dyn ToSql)> = vec![
        (":account", &account),
        (":before_seq", &before_seq),
        (":before_idx", &before_idx),
        (":after_seq", &after_seq),
        (":after_idx", &after_idx),
        (":limit", &limit),
    ];
    let sql = match &query.kind {
        Some(kind) => {
            args.push((":kind", kind));
            PAGE_OF_KIND
        }
        None => PAGE,
    };
    let mut stmt = conn.prepare(sql).context("preparing a history page")?;
    let rows = stmt
        .query_map(args.as_slice(), |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get::<_, String>(3)?)))
        .context("reading a history page")?;
    let mut events = rows
        .map(|row| {
            let (seq, idx, ts, event): (i64, i64, Millis, String) = row.context("reading a history row")?;
            let cursor = Offset { seq: u64::try_from(seq)?, index: u32::try_from(idx)? };
            let event = serde_json::from_str(&event).with_context(|| format!("history event {cursor}"))?;
            Ok(HistoryEvent { cursor, ts, event })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let next = if events.len() > query.limit {
        events.truncate(query.limit);
        events.last().map(|event| event.cursor)
    } else {
        None
    };
    Ok(Page { events, next })
}

/// A cursor as the `(seq, idx)` SQLite compares, or `(absent, absent)` without one. Every stored
/// seq fits an `i64`, so a larger one bounds like the largest.
fn bound(cursor: Option<Offset>, absent: i64) -> (i64, i64) {
    cursor.map_or((absent, absent), |c| (i64::try_from(c.seq).unwrap_or(i64::MAX), i64::from(c.index)))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::scratch_dir;

    #[test]
    fn a_page_reads_its_account_index_in_order_without_sorting() {
        let path = scratch_dir("history-plan").join("history.sqlite");
        let writer = Writer::open(&path, 1).unwrap();
        for (sql, index) in [(PAGE, "events_by_account"), (PAGE_OF_KIND, "events_by_account_kind")] {
            let mut explain = writer.conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
            let mut rows = explain.raw_query();
            let mut plan: Vec<String> = Vec::new();
            while let Some(row) = rows.next().unwrap() {
                plan.push(row.get(3).unwrap());
            }
            // One search of the index, bounded on both sides by the cursors, in index order.
            assert_eq!(plan.len(), 1, "{plan:?}");
            assert!(plan[0].starts_with(&format!("SEARCH events USING INDEX {index} (")), "{plan:?}");
            assert!(plan[0].contains("(seq,idx)>(?,?) AND (seq,idx)<(?,?)"), "{plan:?}");
        }
    }
}
