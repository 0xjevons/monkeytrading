//! The public event log: one JSON object per line. Command entries are the engine's entire input.
//! Checkpoint entries carry the state root after a given seq, signed by the enclave key. The same
//! entries stream live over `/v1/stream`.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;

use anyhow::Context;
use engine_core::command::Sequenced;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "entry", rename_all = "snake_case")]
pub enum LogEntry {
    Command(Sequenced),
    Checkpoint(Checkpoint),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub seq: u64,
    /// 0x-prefixed keccak-256 state root.
    pub root: String,
    /// Enclave signature over the root; see `tee::EnclaveKey::sign_checkpoint`.
    pub signature: String,
    pub signer: String,
}

/// Appends entries to the log file, buffered until `flush`.
pub struct LogWriter {
    out: BufWriter<File>,
}

impl LogWriter {
    /// Opens the log for appending, creating it and its directory if missing.
    /// Error cases:
    /// - the directory cannot be created, or the file cannot be opened for appending
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening log {}", path.display()))?;
        Ok(Self { out: BufWriter::new(file) })
    }

    /// Buffers one entry as a JSON line.
    /// Error cases:
    /// - the write to the buffer or file fails
    pub fn append(&mut self, entry: &LogEntry) -> anyhow::Result<()> {
        serde_json::to_writer(&mut self.out, entry).context("writing log entry")?;
        self.out.write_all(b"\n").context("writing log entry")?;
        Ok(())
    }

    /// Writes buffered entries to the file. Called after each command batch: a command must be
    /// durable before its effects are acknowledged to clients.
    /// Error cases:
    /// - the write to the file fails
    pub fn flush(&mut self) -> anyhow::Result<()> {
        self.out.flush().context("flushing log")
    }
}

/// Reads every entry of a log file; a missing file is an empty log.
/// Error cases:
/// - the file exists but cannot be read
/// - a non-blank line is not a valid entry
pub fn read_all(path: &Path) -> anyhow::Result<Vec<LogEntry>> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("opening log {}", path.display())),
    };
    BufReader::new(file)
        .lines()
        .enumerate()
        .filter(|(_, line)| !matches!(line, Ok(l) if l.trim().is_empty()))
        .map(|(n, line)| {
            let line = line.with_context(|| format!("reading log line {}", n + 1))?;
            serde_json::from_str(&line).with_context(|| format!("log line {}", n + 1))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::command::Command;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("engine-log-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("log.jsonl")
    }

    #[test]
    fn entries_round_trip_through_the_file() {
        let path = scratch("round-trip");
        let mut writer = LogWriter::open(&path).unwrap();
        let cp = Checkpoint { seq: 0, root: "0x00".into(), signature: "0x01".into(), signer: "0x02".into() };
        writer.append(&LogEntry::Command(Sequenced { seq: 0, ts: 5, cmd: Command::Tick })).unwrap();
        writer.append(&LogEntry::Checkpoint(cp.clone())).unwrap();
        writer.flush().unwrap();

        let entries = read_all(&path).unwrap();
        assert!(matches!(&entries[0], LogEntry::Command(s) if s.seq == 0 && s.ts == 5));
        assert!(matches!(&entries[1], LogEntry::Checkpoint(c) if *c == cp));
    }

    #[test]
    fn a_missing_log_is_empty_and_a_corrupt_line_is_an_error() {
        let path = scratch("corrupt");
        assert!(read_all(&path).unwrap().is_empty());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "\n{\"entry\":\"command\",\"seq\":0,\"ts\":0,\"cmd\":{\"type\":\"tick\"}}\nnot json\n")
            .unwrap();
        let err = read_all(&path).unwrap_err();
        assert!(format!("{err:#}").contains("log line 3"), "{err:#}");
    }
}
