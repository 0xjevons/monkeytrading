//! The log on disk: a directory of segment files, each named for the seq of its first command
//! (`00000000000000000000.jsonl`), and `manifest.json`. The writer appends to the last segment and
//! starts a new one at a command once the current one holds `segment_commands` commands, so a
//! seq's entries never straddle two segments. A completed segment is never written again; the
//! manifest records its seq range, size, and SHA-256. Completed segments and the manifest are the
//! public archive.

use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::log::{LogEntry, parse_lines};

pub const MANIFEST: &str = "manifest.json";

/// The completed segments, oldest first.
#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub segments: Vec<SegmentInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SegmentInfo {
    pub file: String,
    pub first_seq: u64,
    pub last_seq: u64,
    pub bytes: u64,
    /// Hex SHA-256 of the file.
    pub sha256: String,
}

/// A log directory read in full, ready to resume writing.
pub struct SegmentedLog {
    dir: PathBuf,
    /// Every entry, oldest first.
    pub entries: Vec<LogEntry>,
    manifest: Manifest,
    /// The last segment, when the manifest does not list it yet.
    open: Option<Segment>,
}

/// Appends entries to the segment directory, buffered until `flush`.
pub struct LogWriter {
    dir: PathBuf,
    segment_commands: u64,
    manifest: Manifest,
    open: Option<(Segment, BufWriter<File>)>,
}

/// What the manifest needs to know about a segment, kept up to date as it is written.
struct Segment {
    first_seq: u64,
    last_seq: u64,
    commands: u64,
    bytes: u64,
    hasher: Sha256,
}

impl Manifest {
    /// Reads the manifest in `dir`; a missing one lists nothing.
    /// Error cases:
    /// - the file exists but cannot be read, or is not a valid manifest
    pub fn load(dir: &Path) -> anyhow::Result<Self> {
        let path = dir.join(MANIFEST);
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Replaces the manifest in `dir` atomically and durably.
    /// Error cases:
    /// - the temporary file cannot be written or synced, or renamed over the manifest
    fn store(&self, dir: &Path) -> anyhow::Result<()> {
        let tmp = dir.join(format!("{MANIFEST}.tmp"));
        let mut file = File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        serde_json::to_writer_pretty(&mut file, self).context("writing manifest")?;
        file.write_all(b"\n").context("writing manifest")?;
        file.sync_all().context("syncing manifest")?;
        fs::rename(&tmp, dir.join(MANIFEST)).context("replacing manifest")?;
        File::open(dir).and_then(|d| d.sync_all()).with_context(|| format!("syncing {}", dir.display()))
    }
}

impl SegmentedLog {
    /// Reads every segment in `dir` in order and checks completed ones against the manifest. A
    /// missing directory is an empty log.
    /// Error cases:
    /// - the directory, the manifest, or a segment cannot be read
    /// - a segment ends mid-line, holds an invalid entry, or does not start with the command for
    ///   the seq it is named for
    /// - the manifest lists a missing segment, or one whose seq range, size, or hash differs
    /// - a segment other than the last is missing from the manifest, or an empty last segment is
    ///   not named for the next seq
    pub fn open(dir: &Path) -> anyhow::Result<Self> {
        let manifest = Manifest::load(dir)?;
        let files = list_segments(dir)?;
        ensure!(
            manifest.segments.len() <= files.len(),
            "{MANIFEST} lists {} segments but {} are in {}",
            manifest.segments.len(),
            files.len(),
            dir.display()
        );
        let mut entries = Vec::new();
        let mut open = None;
        for (i, (first_seq, path)) in files.iter().enumerate() {
            let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
            let parsed = parse_segment(&bytes).with_context(|| format!("segment {}", path.display()))?;
            let segment = Segment::scan(*first_seq, &bytes, &parsed).with_context(|| format!("{}", path.display()))?;
            match manifest.segments.get(i) {
                Some(listed) => ensure!(
                    segment.info() == *listed,
                    "{} does not match {MANIFEST}: on disk {:?}, listed {listed:?}",
                    path.display(),
                    segment.info()
                ),
                None if i + 1 == files.len() => open = Some(segment),
                None => bail!("{} is followed by another segment but is not in {MANIFEST}", path.display()),
            }
            entries.extend(parsed);
        }
        if let Some(segment) = open.as_ref().filter(|s| s.commands == 0) {
            let next_seq = entries.iter().rev().find_map(command_seq).map_or(0, |s| s + 1);
            ensure!(segment.first_seq == next_seq, "empty last segment is not named for the next seq {next_seq}");
        }
        Ok(Self { dir: dir.to_owned(), entries, manifest, open })
    }

    /// A writer that appends to the last segment, or starts a new one at the next command if the
    /// manifest lists the last one.
    /// Error cases:
    /// - the directory cannot be created, or the last segment cannot be opened for appending
    pub fn into_writer(self, segment_commands: u64) -> anyhow::Result<LogWriter> {
        fs::create_dir_all(&self.dir).with_context(|| format!("creating {}", self.dir.display()))?;
        let open = match self.open {
            Some(segment) => {
                let path = self.dir.join(segment_name(segment.first_seq));
                let file = OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .with_context(|| format!("opening {}", path.display()))?;
                Some((segment, BufWriter::new(file)))
            }
            None => None,
        };
        Ok(LogWriter { dir: self.dir, segment_commands, manifest: self.manifest, open })
    }
}

impl LogWriter {
    /// Buffers one entry as a JSON line. A command first completes the current segment if it is
    /// full.
    /// Error cases:
    /// - completing the current segment or starting a new one fails
    /// - a withdrawal or checkpoint comes before any command in this segment
    /// - the write to the buffer or file fails
    pub fn append(&mut self, entry: &LogEntry) -> anyhow::Result<()> {
        if let Some(seq) = command_seq(entry)
            && self.open.as_ref().is_none_or(|(segment, _)| segment.commands >= self.segment_commands)
        {
            self.start_segment(seq)?;
        }
        let (segment, out) = self.open.as_mut().context("log entry before the first command of its segment")?;
        let mut line = serde_json::to_vec(entry).context("encoding log entry")?;
        line.push(b'\n');
        out.write_all(&line).context("writing log entry")?;
        segment.record(&line, entry);
        Ok(())
    }

    /// Writes buffered entries to the file. Called after each command batch: a command must be
    /// durable before its effects are acknowledged to clients.
    /// Error cases:
    /// - the write to the file fails
    pub fn flush(&mut self) -> anyhow::Result<()> {
        match &mut self.open {
            Some((_, out)) => out.flush().context("flushing log"),
            None => Ok(()),
        }
    }

    /// Syncs and lists the current segment, if any, then creates the segment for `first_seq`.
    /// The new file exists only once the old one is complete, which is what `LogReader` relies on.
    /// Error cases:
    /// - the current segment cannot be flushed or synced, or the manifest cannot be stored
    /// - the new segment cannot be created, or already exists
    fn start_segment(&mut self, first_seq: u64) -> anyhow::Result<()> {
        if let Some((segment, out)) = self.open.take() {
            let file = out.into_inner().map_err(|e| e.into_error()).context("flushing log segment")?;
            file.sync_all().context("syncing log segment")?;
            self.manifest.segments.push(segment.info());
            self.manifest.store(&self.dir)?;
        }
        let path = self.dir.join(segment_name(first_seq));
        let file = OpenOptions::new()
            .append(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("creating {}", path.display()))?;
        self.open = Some((Segment::empty(first_seq), BufWriter::new(file)));
        Ok(())
    }
}

impl Segment {
    /// A new, empty segment for `first_seq`.
    fn empty(first_seq: u64) -> Self {
        Self { first_seq, last_seq: first_seq, commands: 0, bytes: 0, hasher: Sha256::new() }
    }

    /// The segment named for `first_seq`, whose file holds `bytes`, parsed as `entries`.
    /// Error cases:
    /// - the first entry is not the command for `first_seq`
    fn scan(first_seq: u64, bytes: &[u8], entries: &[LogEntry]) -> anyhow::Result<Self> {
        if let Some(first) = entries.first() {
            ensure!(command_seq(first) == Some(first_seq), "does not start with the command for seq {first_seq}");
        }
        let mut segment = Self::empty(first_seq);
        segment.hasher.update(bytes);
        segment.bytes = bytes.len() as u64;
        for seq in entries.iter().filter_map(command_seq) {
            segment.commands += 1;
            segment.last_seq = seq;
        }
        Ok(segment)
    }

    /// Accounts for one line just written.
    fn record(&mut self, line: &[u8], entry: &LogEntry) {
        self.hasher.update(line);
        self.bytes += line.len() as u64;
        if let Some(seq) = command_seq(entry) {
            self.commands += 1;
            self.last_seq = seq;
        }
    }

    /// The manifest entry for this segment as written so far.
    fn info(&self) -> SegmentInfo {
        SegmentInfo {
            file: segment_name(self.first_seq),
            first_seq: self.first_seq,
            last_seq: self.last_seq,
            bytes: self.bytes,
            sha256: hex::encode(self.hasher.clone().finalize()),
        }
    }
}

/// The segment files in `dir`, by first seq; a missing directory has none.
/// Error cases:
/// - the directory exists but cannot be listed
pub(super) fn list_segments(dir: &Path) -> anyhow::Result<Vec<(u64, PathBuf)>> {
    let listing = match fs::read_dir(dir) {
        Ok(listing) => listing,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("listing {}", dir.display())),
    };
    let mut segments = Vec::new();
    for item in listing {
        let name = item.with_context(|| format!("listing {}", dir.display()))?.file_name();
        if let Some(seq) = name.to_str().and_then(segment_seq) {
            segments.push((seq, dir.join(name)));
        }
    }
    segments.sort_unstable_by_key(|(seq, _)| *seq);
    Ok(segments)
}

/// The file name of the segment whose first command is `first_seq`.
fn segment_name(first_seq: u64) -> String {
    format!("{first_seq:020}.jsonl")
}

/// The first seq a segment file name stands for, if it is one.
fn segment_seq(name: &str) -> Option<u64> {
    let seq = name.strip_suffix(".jsonl")?.parse().ok()?;
    (segment_name(seq) == name).then_some(seq)
}

/// The seq of a command entry.
fn command_seq(entry: &LogEntry) -> Option<u64> {
    match entry {
        LogEntry::Command(c) => Some(c.sequenced.seq),
        _ => None,
    }
}

/// Parses a segment file; unlike a single log file, it must end on a line break, since the writer
/// appends to it.
/// Error cases:
/// - the last line has no line break, which a torn write leaves
/// - a non-blank line is not a valid entry
fn parse_segment(bytes: &[u8]) -> anyhow::Result<Vec<LogEntry>> {
    ensure!(bytes.is_empty() || bytes.ends_with(b"\n"), "ends mid-line");
    parse_lines(bytes)
}
