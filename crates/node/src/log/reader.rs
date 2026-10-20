//! Reads the log from any offset, across segments, while the sequencer appends to it. Only whole
//! lines are read: a line still being written is left for a later call.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};

use crate::log::segments::list_segments;
use crate::log::{LogEntry, Offset, offset_of};

/// Entries of the log at or after a starting offset, in order.
pub struct LogReader {
    dir: PathBuf,
    from: Offset,
    segment: Option<OpenSegment>,
    last: Option<Offset>,
}

struct OpenSegment {
    first_seq: u64,
    path: PathBuf,
    file: BufReader<File>,
    /// Bytes of a line not yet fully written.
    line: Vec<u8>,
    /// A later segment exists, so this one gets no more appends.
    complete: bool,
}

impl LogReader {
    /// A reader for the log in `dir` that yields entries at or after `from`, starting in the
    /// segment that holds `from.seq`.
    /// Error cases:
    /// - the directory cannot be listed, or the segment cannot be opened
    pub fn open(dir: &Path, from: Offset) -> anyhow::Result<Self> {
        let segments = list_segments(dir)?;
        let start = segments.iter().rev().find(|(first, _)| *first <= from.seq).or(segments.first());
        let segment = start.map(|(first, path)| OpenSegment::open(*first, path)).transpose()?;
        Ok(Self { dir: dir.to_owned(), from, segment, last: None })
    }

    /// The next entry at or after `from`, with its offset, or `None` once every whole line on disk
    /// has been read. A later call picks up whatever has been written since.
    /// Error cases:
    /// - a segment cannot be listed, opened, or read, or holds a line that is not a valid entry
    /// - a segment does not start with a command, or a completed one ends mid-line
    pub fn next_entry(&mut self) -> anyhow::Result<Option<(Offset, LogEntry)>> {
        loop {
            let Some(segment) = self.segment.as_mut() else {
                // The log was empty when this reader opened.
                let Some((first, path)) = list_segments(&self.dir)?.into_iter().next() else {
                    return Ok(None);
                };
                self.segment = Some(OpenSegment::open(first, &path)?);
                continue;
            };
            if let Some(entry) = segment.read_entry()? {
                let offset = offset_of(self.last, &entry).with_context(|| segment.path.display().to_string())?;
                self.last = Some(offset);
                if offset >= self.from {
                    return Ok(Some((offset, entry)));
                }
                continue;
            }
            // The writer creates a segment only after the previous one is written in full, so once
            // a later one exists, one more pass over this one reaches its true end.
            let later = list_segments(&self.dir)?.into_iter().find(|(first, _)| *first > segment.first_seq);
            match later {
                None => return Ok(None),
                Some(_) if !segment.complete => segment.complete = true,
                Some((first, path)) => {
                    ensure!(segment.line.is_empty(), "{} ends mid-line", segment.path.display());
                    self.segment = Some(OpenSegment::open(first, &path)?);
                }
            }
        }
    }
}

impl OpenSegment {
    /// Opens the segment at `path`, named for `first_seq`.
    /// Error cases:
    /// - the file cannot be opened
    fn open(first_seq: u64, path: &Path) -> anyhow::Result<Self> {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        Ok(Self { first_seq, path: path.to_owned(), file: BufReader::new(file), line: Vec::new(), complete: false })
    }

    /// The entry on the next whole line, skipping blank ones, or `None` if no further line is
    /// fully written yet.
    /// Error cases:
    /// - the file cannot be read, or the line is not a valid entry
    fn read_entry(&mut self) -> anyhow::Result<Option<LogEntry>> {
        loop {
            self.file.read_until(b'\n', &mut self.line).with_context(|| format!("reading {}", self.path.display()))?;
            if self.line.last() != Some(&b'\n') {
                return Ok(None);
            }
            let line = std::mem::take(&mut self.line);
            if !line.trim_ascii().is_empty() {
                let entry =
                    serde_json::from_slice(&line).with_context(|| format!("entry in {}", self.path.display()))?;
                return Ok(Some(entry));
            }
        }
    }
}
