//! Reads a completed segment in fixed-size chunks, each checked against the hash taken when the
//! whole file was first checked against its manifest listing. A reader that stops and resumes then
//! needs neither the whole segment in memory nor another pass over it.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, ensure};
use sha2::{Digest, Sha256};

use crate::log::{MANIFEST, SegmentInfo};

/// Bytes per chunk; small under test, so tests cross chunk boundaries.
const CHUNK_BYTES: usize = if cfg!(test) { 64 } else { 1 << 20 };

/// The SHA-256 of each chunk of a segment that matched its listing.
pub(super) type ChunkHashes = Arc<[[u8; 32]]>;

/// A completed segment's bytes from some position on, each chunk checked as it is loaded.
pub(super) struct CheckedChunks {
    file: File,
    chunks: ChunkHashes,
    next_chunk: usize,
    buf: Vec<u8>,
    pos: usize,
}

impl Read for CheckedChunks {
    /// Bytes from the current chunk, loading and checking the next one when it runs out.
    /// Error cases:
    /// - the next chunk cannot be read, or differs from its hash
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos == self.buf.len() {
            self.load()?;
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl CheckedChunks {
    /// The segment at `path`, whose chunks hash to `chunks`, from byte `position` on.
    /// Error cases:
    /// - the file cannot be opened or positioned, or the chunk holding `position` differs
    pub(super) fn open(path: &Path, chunks: ChunkHashes, position: u64) -> anyhow::Result<Self> {
        let first = position / CHUNK_BYTES as u64;
        let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        file.seek(SeekFrom::Start(first * CHUNK_BYTES as u64))
            .with_context(|| format!("seeking in {}", path.display()))?;
        let next_chunk = usize::try_from(first).context("segment position")?;
        let mut reader = Self { file, chunks, next_chunk, buf: Vec::new(), pos: 0 };
        reader.load().with_context(|| format!("reading {}", path.display()))?;
        reader.pos = (position % CHUNK_BYTES as u64) as usize;
        Ok(reader)
    }

    /// Loads and checks the next chunk; leaves `buf` empty past the last one.
    /// Error cases:
    /// - the file cannot be read, or the chunk's bytes differ from its hash
    fn load(&mut self) -> io::Result<()> {
        self.buf.clear();
        self.pos = 0;
        let Some(expected) = self.chunks.get(self.next_chunk) else {
            return Ok(());
        };
        self.buf.resize(CHUNK_BYTES, 0);
        let n = read_full(&mut self.file, &mut self.buf)?;
        self.buf.truncate(n);
        if <[u8; 32]>::from(Sha256::digest(&self.buf)) != *expected {
            let msg = format!("chunk {} does not match its listing in {MANIFEST}", self.next_chunk);
            return Err(io::Error::new(io::ErrorKind::InvalidData, msg));
        }
        self.next_chunk += 1;
        Ok(())
    }
}

/// Checks the segment at `path` against `listed` in one pass, and returns each chunk's hash.
/// Error cases:
/// - the file cannot be opened or read
/// - its size or SHA-256 differs from `listed`
pub(super) fn check(path: &Path, listed: &SegmentInfo) -> anyhow::Result<ChunkHashes> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let (mut whole, mut chunks, mut bytes) = (Sha256::new(), Vec::new(), 0u64);
    let mut buf = vec![0; CHUNK_BYTES];
    loop {
        let n = read_full(&mut file, &mut buf).with_context(|| format!("reading {}", path.display()))?;
        if n == 0 {
            break;
        }
        whole.update(&buf[..n]);
        chunks.push(Sha256::digest(&buf[..n]).into());
        bytes += n as u64;
    }
    ensure!(
        bytes == listed.bytes && hex::encode(whole.finalize()) == listed.sha256,
        "{} does not match its listing in {MANIFEST}",
        path.display()
    );
    Ok(chunks.into())
}

/// Fills `buf` from `file`, short only at the end of the file.
/// Error cases:
/// - the file cannot be read
fn read_full(file: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}
