//! The public event log: one JSON object per line, split into segment files. Command entries are
//! the engine's entire input, with the events each produced. Withdrawal entries carry the enclave's
//! signature on each approved withdrawal. Checkpoint entries carry the state root after a given seq,
//! signed by the enclave key. The same entries stream live over `/v1/stream`, each at an
//! [`Offset`].

mod checked;
mod reader;
pub use reader::LogReader;
mod segments;
pub(crate) use segments::segment_seq;
pub use segments::{LogWriter, MANIFEST, Manifest, SegmentInfo, SegmentedLog};

use std::fmt;
use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, bail};
use engine_core::command::Sequenced;
use engine_core::event::Event;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::sequencer::SignedWithdrawal;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "entry", rename_all = "snake_case")]
pub enum LogEntry {
    Command(CommandEntry),
    Checkpoint(Checkpoint),
    Withdrawal(SignedWithdrawal),
}

/// A sequenced command and the events applying it produced.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CommandEntry {
    #[serde(flatten)]
    pub sequenced: Sequenced,
    /// `None` only in entries written before events were logged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub events: Option<Vec<Event>>,
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

/// Where a message sits in the log: its seq, and its index among that seq's entries (the command
/// is 0, then its withdrawals, then any checkpoint). Written `<seq>:<index>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Offset {
    pub seq: u64,
    pub index: u32,
}

impl fmt::Display for Offset {
    /// `<seq>:<index>`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.seq, self.index)
    }
}

impl FromStr for Offset {
    type Err = anyhow::Error;

    /// Parses `<seq>:<index>`.
    /// Error cases:
    /// - no `:`, or either side is not an unsigned integer in range
    fn from_str(s: &str) -> anyhow::Result<Self> {
        let (seq, index) = s.split_once(':').with_context(|| format!("offset {s:?} is not <seq>:<index>"))?;
        Ok(Self {
            seq: seq.parse().with_context(|| format!("offset {s:?}: seq"))?,
            index: index.parse().with_context(|| format!("offset {s:?}: index"))?,
        })
    }
}

impl Serialize for Offset {
    /// As its `<seq>:<index>` string.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Offset {
    /// From a `<seq>:<index>` string.
    /// Error cases:
    /// - the value is not a string in that form
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?.parse().map_err(|e| D::Error::custom(format!("{e:#}")))
    }
}

impl Offset {
    /// The first message of `seq`.
    pub fn start_of(seq: u64) -> Self {
        Self { seq, index: 0 }
    }

    /// The position right after this one within the same seq.
    pub fn next(self) -> Self {
        Self { seq: self.seq, index: self.index.saturating_add(1) }
    }
}

/// The offset of `entry`, given the offset of the entry before it. Every segment starts with a
/// command, so `prev` is `None` only there.
/// Error cases:
/// - a withdrawal or checkpoint with no entry before it
pub fn offset_of(prev: Option<Offset>, entry: &LogEntry) -> anyhow::Result<Offset> {
    match (entry, prev) {
        (LogEntry::Command(c), _) => Ok(Offset::start_of(c.sequenced.seq)),
        (_, Some(prev)) => Ok(prev.next()),
        (_, None) => bail!("log entry before the first command of its segment"),
    }
}

/// The offset of the last of `entries`, a whole log in order.
/// Error cases:
/// - a withdrawal or checkpoint comes before any command
pub fn last_offset(entries: &[LogEntry]) -> anyhow::Result<Option<Offset>> {
    entries.iter().try_fold(None, |prev, entry| offset_of(prev, entry).map(Some))
}

/// Reads every entry of a single log file; a missing file is an empty log.
/// Error cases:
/// - the file exists but cannot be read
/// - a non-blank line is not a valid entry
pub fn read_all(path: &Path) -> anyhow::Result<Vec<LogEntry>> {
    match std::fs::read(path) {
        Ok(bytes) => parse_lines(&bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("opening log {}", path.display())),
    }
}

/// Parses JSON lines, skipping blank ones.
/// Error cases:
/// - a non-blank line is not a valid entry
fn parse_lines(bytes: &[u8]) -> anyhow::Result<Vec<LogEntry>> {
    bytes
        .split(|b| *b == b'\n')
        .enumerate()
        .filter(|(_, line)| !line.trim_ascii().is_empty())
        .map(|(n, line)| serde_json::from_slice(line).with_context(|| format!("log line {}", n + 1)))
        .collect()
}

#[cfg(test)]
mod tests;
