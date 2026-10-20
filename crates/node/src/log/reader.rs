//! Reads the log from any offset, across segments, while the sequencer appends to it. Only whole
//! lines are read: a line still being written is left for a later call.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Cursor};
use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use crate::log::segments::list_segments;
use crate::log::{LogEntry, MANIFEST, Manifest, Offset, offset_of};

/// Entries of the log at or after a starting offset, in order.
pub struct LogReader {
    dir: PathBuf,
    from: Offset,
    /// The writer's manifest, which completed segments are checked against.
    manifest: watch::Receiver<Manifest>,
    /// Segment files by first seq, as last listed.
    segments: Vec<(u64, PathBuf)>,
    /// The segment being read, by index into `segments`.
    segment: Option<(usize, OpenSegment)>,
    last: Option<Offset>,
}

struct OpenSegment {
    path: PathBuf,
    lines: Box<dyn BufRead + Send>,
    /// Bytes of a line not yet fully written.
    line: Vec<u8>,
    /// A later segment was listed before this one was last read to its end, so it gets no more
    /// appends.
    complete: bool,
    /// An entry has been read, so the segment's first entry has been checked.
    started: bool,
}

impl LogReader {
    /// A reader for the log in `dir` that yields entries at or after `from`, starting in the
    /// segment that holds `from.seq`, and checks completed segments against `manifest`.
    /// Error cases:
    /// - the directory cannot be listed, or the start segment is missing or cannot be opened (see
    ///   `open_start`)
    pub fn open(dir: &Path, from: Offset, manifest: watch::Receiver<Manifest>) -> anyhow::Result<Self> {
        let segments = list_segments(dir)?;
        let mut reader = Self { dir: dir.to_owned(), from, manifest, segments, segment: None, last: None };
        reader.open_start()?;
        Ok(reader)
    }

    /// The next entry at or after `from`, with its offset, or `None` once every whole line on disk
    /// has been read. A later call picks up whatever has been written since.
    /// Error cases:
    /// - a segment cannot be listed, opened, or read, or holds a line that is not a valid entry
    /// - a completed segment differs from its listing, or ends mid-line
    /// - a segment does not start with a command
    pub fn next_entry(&mut self) -> anyhow::Result<Option<(Offset, LogEntry)>> {
        loop {
            let Some((i, segment)) = self.segment.as_mut() else {
                // The log was empty when last listed.
                self.segments = list_segments(&self.dir)?;
                self.open_start()?;
                if self.segment.is_none() {
                    return Ok(None);
                }
                continue;
            };
            if let Some(entry) = segment.read_entry()? {
                if !segment.started {
                    ensure!(
                        matches!(entry, LogEntry::Command(_)),
                        "{} does not start with a command",
                        segment.path.display()
                    );
                    segment.started = true;
                }
                // A missing segment, or lines removed from one, shows up as a gap in the seqs.
                if let (LogEntry::Command(c), Some(prev)) = (&entry, self.last) {
                    let seq = c.sequenced.seq;
                    ensure!(
                        prev.seq.checked_add(1) == Some(seq),
                        "{}: seq {seq} follows seq {}",
                        segment.path.display(),
                        prev.seq
                    );
                }
                let offset = offset_of(self.last, &entry).with_context(|| segment.path.display().to_string())?;
                self.last = Some(offset);
                if offset >= self.from {
                    return Ok(Some((offset, entry)));
                }
                continue;
            }
            let i = *i;
            if !segment.complete {
                // The writer creates a segment only after the one before it is written in full, so
                // once a later one is listed, one more pass over this one reaches its true end.
                self.segments = list_segments(&self.dir)?;
                if i + 1 >= self.segments.len() {
                    return Ok(None);
                }
                segment.complete = true;
                continue;
            }
            ensure!(segment.line.is_empty(), "{} ends mid-line", segment.path.display());
            // Release this segment before the next is read into memory.
            self.segment = None;
            self.segment = Some((i + 1, self.open_segment(i + 1)?));
        }
    }

    /// Opens the segment that holds `from.seq`, once the log has any segment.
    /// Error cases:
    /// - segments exist but none holds `from.seq`, so the start of the log is missing
    /// - the segment cannot be opened (see `open_segment`)
    fn open_start(&mut self) -> anyhow::Result<()> {
        let Some(i) = self.segments.iter().rposition(|(first, _)| *first <= self.from.seq) else {
            ensure!(self.segments.is_empty(), "no segment in {} holds seq {}", self.dir.display(), self.from.seq);
            return Ok(());
        };
        self.segment = Some((i, self.open_segment(i)?));
        Ok(())
    }

    /// Opens segment `i`. A segment the manifest lists is read whole and checked against its
    /// listing before any of it is yielded, since the host can rewrite files on disk.
    /// Error cases:
    /// - the file cannot be opened or read
    /// - a listed segment's size or SHA-256 differs from its listing
    fn open_segment(&self, i: usize) -> anyhow::Result<OpenSegment> {
        let (first_seq, path) = &self.segments[i];
        let listed = self.manifest.borrow().get(*first_seq).cloned();
        let lines: Box<dyn BufRead + Send> = match listed {
            Some(listed) => {
                let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
                let sha256 = hex::encode(Sha256::digest(&bytes));
                ensure!(
                    bytes.len() as u64 == listed.bytes && sha256 == listed.sha256,
                    "{} does not match its listing in {MANIFEST}",
                    path.display()
                );
                Box::new(Cursor::new(bytes))
            }
            // TODO: the segment still being written is read from host disk unchecked, to its end even
            // if it completes meanwhile. Check it against the writer's running hash, or serve it from
            // enclave memory, before relying on backfill for integrity; until then, replay against
            // the signed checkpoints is the check.
            None => {
                let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
                Box::new(BufReader::new(file))
            }
        };
        let complete = i + 1 < self.segments.len();
        Ok(OpenSegment { path: path.clone(), lines, line: Vec::new(), complete, started: false })
    }
}

impl OpenSegment {
    /// The entry on the next whole line, skipping blank ones, or `None` if no further line is
    /// fully written yet.
    /// Error cases:
    /// - the file cannot be read, or the line is not a valid entry
    fn read_entry(&mut self) -> anyhow::Result<Option<LogEntry>> {
        loop {
            self.lines.read_until(b'\n', &mut self.line).with_context(|| format!("reading {}", self.path.display()))?;
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
