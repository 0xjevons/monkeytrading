//! Read-only history planning before authenticated log and custody preflight permits writes.

use std::path::PathBuf;

use anyhow::{Context, ensure};
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
            // Immutable SQLite ignores WAL. A crash WAL is conservatively rebuilt after preflight,
            // rather than opening its shared-memory file for writes before authorization.
            ensure!(!path.with_file_name(format!("{INDEX_FILE}-wal")).exists(), "history has a recovery WAL");
            let mut uri = reqwest::Url::from_file_path(std::path::absolute(&path)?)
                .map_err(|_| anyhow::anyhow!("history path is not absolute"))?;
            uri.query_pairs_mut().append_pair("immutable", "1");
            let conn = Connection::open_with_flags(
                uri.as_str(),
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
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
