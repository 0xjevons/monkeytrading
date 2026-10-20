//! The public event log: one JSON object per line. Command entries are the
//! engine's entire input. Checkpoint entries carry the state root after a
//! given seq, signed by the enclave key. The same entries stream live over
//! `/v1/stream`.

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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Checkpoint {
    pub seq: u64,
    /// 0x-prefixed keccak-256 state root.
    pub root: String,
    /// Enclave signature over the root; see `tee::EnclaveKey::sign_checkpoint`.
    pub signature: String,
    pub signer: String,
}

pub struct LogWriter {
    out: BufWriter<File>,
}

impl LogWriter {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening log {}", path.display()))?;
        Ok(Self { out: BufWriter::new(file) })
    }

    pub fn append(&mut self, entry: &LogEntry) -> anyhow::Result<()> {
        serde_json::to_writer(&mut self.out, entry)?;
        self.out.write_all(b"\n")?;
        Ok(())
    }

    /// Called after each command batch. A command must be durable before
    /// its effects are acknowledged to clients.
    pub fn flush(&mut self) -> anyhow::Result<()> {
        self.out.flush()?;
        Ok(())
    }
}

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
            let line = line?;
            serde_json::from_str(&line).with_context(|| format!("log line {}", n + 1))
        })
        .collect()
}
