//! Reads the log from any offset, across segments, while the sequencer appends to it. Only whole
//! lines are read: a line still being written is left for a later call.

use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};
use tokio::sync::watch;

use crate::log::checked::{self, CheckedChunks, ChunkHashes};
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
    first_seq: u64,
    path: PathBuf,
    /// The segment's contents from `consumed` on; `None` until read, and after `release`.
    lines: Option<Box<dyn BufRead + Send>>,
    /// Chunk hashes, once a listed segment has matched its listing.
    checked: Option<ChunkHashes>,
    /// Bytes of whole lines read so far.
    consumed: u64,
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
    /// - the directory cannot be listed, or segments exist but none holds `from.seq`
    pub fn open(dir: &Path, from: Offset, manifest: watch::Receiver<Manifest>) -> anyhow::Result<Self> {
        let segments = list_segments(dir)?;
        let mut reader = Self { dir: dir.to_owned(), from, manifest, segments, segment: None, last: None };
        reader.open_start()?;
        Ok(reader)
    }

    /// Closes the reader's segment file. The next read opens it again where this one stopped, and
    /// checks each chunk it reads once the manifest lists the segment.
    pub fn release(&mut self) {
        if let Some((_, segment)) = &mut self.segment {
            segment.lines = None;
            segment.line.clear();
        }
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
            if segment.lines.is_none() {
                segment.lines = Some(segment_lines(&self.manifest, segment)?);
            }
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
            self.segment = Some((i + 1, self.open_segment(i + 1)));
        }
    }

    /// Opens the segment that holds `from.seq`, once the log has any segment.
    /// Error cases:
    /// - segments exist but none holds `from.seq`, so the start of the log is missing
    fn open_start(&mut self) -> anyhow::Result<()> {
        let Some(i) = self.segments.iter().rposition(|(first, _)| *first <= self.from.seq) else {
            ensure!(self.segments.is_empty(), "no segment in {} holds seq {}", self.dir.display(), self.from.seq);
            return Ok(());
        };
        self.segment = Some((i, self.open_segment(i)));
        Ok(())
    }

    /// Segment `i`, to be read from its start.
    fn open_segment(&self, i: usize) -> OpenSegment {
        let (first_seq, path) = &self.segments[i];
        let complete = i + 1 < self.segments.len();
        let (lines, checked, consumed, line, started) = (None, None, 0, Vec::new(), false);
        OpenSegment { first_seq: *first_seq, path: path.clone(), lines, checked, consumed, line, complete, started }
    }
}

impl OpenSegment {
    /// The entry on the next whole line, skipping blank ones, or `None` if no further line is
    /// fully written yet.
    /// Error cases:
    /// - the file cannot be read, or the line is not a valid entry
    fn read_entry(&mut self) -> anyhow::Result<Option<LogEntry>> {
        let lines = self.lines.as_mut().expect("a segment's contents are opened before it is read");
        loop {
            lines.read_until(b'\n', &mut self.line).with_context(|| format!("reading {}", self.path.display()))?;
            if self.line.last() != Some(&b'\n') {
                return Ok(None);
            }
            let line = std::mem::take(&mut self.line);
            self.consumed += line.len() as u64;
            if !line.trim_ascii().is_empty() {
                let entry =
                    serde_json::from_slice(&line).with_context(|| format!("entry in {}", self.path.display()))?;
                return Ok(Some(entry));
            }
        }
    }
}

/// The contents of `segment` from where its last read stopped. A segment the manifest lists is
/// checked against its listing in one pass the first time it is read, and each chunk after that
/// as it is read, since the host can rewrite files on disk.
/// Error cases:
/// - the file cannot be opened, read, or positioned
/// - a listed segment differs from its listing
/// - an unlisted segment does not start right after the last listed one
fn segment_lines(
    manifest: &watch::Receiver<Manifest>,
    segment: &mut OpenSegment,
) -> anyhow::Result<Box<dyn BufRead + Send>> {
    let path = &segment.path;
    if segment.checked.is_none() {
        let (listed, unlisted_from) = {
            let manifest = manifest.borrow();
            let unlisted_from = manifest.segments.last().map_or(0, |last| last.last_seq + 1);
            (manifest.get(segment.first_seq).cloned(), unlisted_from)
        };
        match listed {
            Some(listed) => segment.checked = Some(checked::check(path, &listed)?),
            None => {
                ensure!(
                    segment.first_seq == unlisted_from,
                    "{} is not in {MANIFEST}, and does not start right after the last segment it lists",
                    path.display()
                );
                // TODO: the segment still being written is read from host disk unchecked, to the
                // end of the batch even if it completes meanwhile. Check it against the writer's
                // running hash, or serve it from enclave memory, before relying on backfill for
                // integrity; until then, replay against the signed checkpoints is the check.
                let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
                let at = SeekFrom::Start(segment.consumed);
                file.seek(at).with_context(|| format!("seeking in {}", path.display()))?;
                return Ok(Box::new(BufReader::new(file)));
            }
        }
    }
    let chunks = segment.checked.clone().expect("a listed segment's chunks are checked above");
    Ok(Box::new(BufReader::new(CheckedChunks::open(path, chunks, segment.consumed)?)))
}
