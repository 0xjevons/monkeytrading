//! Read-only history planning before authenticated log and custody preflight permits writes.

use std::path::PathBuf;

use anyhow::{Context, ensure};
use rusqlite::config::DbConfig;
use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::config::HistorySection;
use crate::history::store::{check_schema, expected_schema, harden};
use crate::history::{History, HistoryCheck, INDEX_FILE};
use crate::log::{CommandEntry, LogEntry};

pub(crate) struct HistoryPlan {
    dir: PathBuf,
    retention_ms: engine_core::types::Millis,
    progress: anyhow::Result<Option<CommandEntry>>,
    found: Option<bool>,
}

impl HistoryPlan {
    /// Plans history recovery without creating or updating any SQLite files, including WAL/SHM.
    /// Error cases:
    /// - the configured retention is invalid
    pub fn open(cfg: &HistorySection) -> anyhow::Result<Self> {
        let retention_ms = cfg.retention_ms()?;
        let path = cfg.dir.join(INDEX_FILE);
        let progress = (|| {
            if !path.exists() {
                return Ok(None);
            }
            let metadata = std::fs::metadata(&path).context("reading history database metadata")?;
            ensure!(metadata.is_file(), "history database is not a regular file");
            // SQLite may remove an existing WAL while opening a zero-page main database,
            // even on a read-only connection. Such an image must wait for authorized repair.
            ensure!(metadata.len() > 0, "history database is empty");
            // A read-only no-lock VFS plus EXCLUSIVE mode gives SQLite a private heap WAL
            // index. It reads committed WAL pages without creating/updating the original SHM,
            // unlike an ordinary read-only open. Actual writable progress is rechecked below.
            // SQLite documents this mode at https://sqlite.org/wal.html#noshm.
            let mut uri = reqwest::Url::from_file_path(std::path::absolute(&path)?)
                .map_err(|_| anyhow::anyhow!("history path is not absolute"))?;
            let has_wal =
                path.with_file_name(format!("{INDEX_FILE}-wal")).try_exists().context("checking history WAL")?;
            if has_wal {
                uri.query_pairs_mut().append_pair("vfs", "unix-none");
            } else {
                // SQLite may create an empty WAL even for a read-only normal WAL-mode open.
                // With no existing WAL, immutable mode reads the complete database itself.
                uri.query_pairs_mut().append_pair("immutable", "1");
            }
            let conn = Connection::open_with_flags(
                uri.as_str(),
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            if has_wal {
                conn.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)
                    .context("disabling preflight close checkpoints")?;
                conn.pragma_update(None, "locking_mode", "exclusive").context("using a private read-only WAL index")?;
            }
            harden(&conn)?;
            check_schema(&conn, &expected_schema(), &path)?;
            let progress: Option<(String, engine_core::types::Millis)> = conn
                .query_row("SELECT command, retention_ms FROM progress WHERE id = 0", [], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })
                .optional()?;
            progress
                .map(|(command, retention)| {
                    ensure!(retention == retention_ms, "history retention differs from config");
                    serde_json::from_str(&command).context("reading history progress")
                })
                .transpose()
        })();
        Ok(Self { dir: cfg.dir.clone(), retention_ms, progress, found: None })
    }

    /// Matches progress against the same authenticated stream that recovers the engine.
    pub fn observe(&mut self, entry: &LogEntry) {
        if let (LogEntry::Command(command), Ok(Some(last))) = (entry, &self.progress)
            && command.sequenced.seq == last.sequenced.seq
        {
            self.found = Some(command == last);
        }
    }

    /// Opens or repairs history only after the caller has authorized persisted mutations.
    /// Error cases:
    /// - SQLite open or authorized repair fails
    pub fn finish(self) -> anyhow::Result<History> {
        let mut check = HistoryCheck::open(&HistorySection {
            dir: self.dir,
            retention: std::time::Duration::from_millis(u64::try_from(self.retention_ms)?),
        })?;
        match self.progress {
            Err(reason) => check.opened = Err(reason),
            Ok(expected) => {
                check.opened = check.opened.and_then(|(writer, actual)| {
                    ensure!(actual.as_ref() == expected.as_ref(), "history progress changed during preflight");
                    Ok((writer, actual))
                });
                check.found = self.found;
                if self.found == Some(true)
                    && let Some(last) = expected
                {
                    check.observe(&LogEntry::Command(last));
                }
            }
        }
        check.finish()
    }
}

#[cfg(test)]
mod tests;
