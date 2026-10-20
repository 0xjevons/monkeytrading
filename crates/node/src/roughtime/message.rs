//! The Roughtime wire format (RFC 10049 §4 and §5): a packet frames one message, and a message
//! maps four-byte tags to byte values, sorted by tag.

use std::fmt;

use anyhow::{Context, anyhow, bail, ensure};

/// The first eight bytes of every packet.
const PACKET_MAGIC: &[u8; 8] = b"ROUGHTIM";
const PACKET_HEADER_LEN: usize = 12;

/// A message tag: one to four ASCII capitals, zero-padded, ordered as a little-endian uint32.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Tag(u32);

/// A parsed message: its tags, ascending, each with its value.
pub struct Message<'a> {
    pairs: Vec<(Tag, &'a [u8])>,
}

impl fmt::Debug for Tag {
    /// The tag's letters, e.g. `NONC`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(String::from_utf8_lossy(&self.0.to_le_bytes()).trim_end_matches('\0'))
    }
}

impl Tag {
    pub const CERT: Tag = Tag::new(*b"CERT");
    pub const DELE: Tag = Tag::new(*b"DELE");
    pub const INDX: Tag = Tag::new(*b"INDX");
    pub const MAXT: Tag = Tag::new(*b"MAXT");
    pub const MIDP: Tag = Tag::new(*b"MIDP");
    pub const MINT: Tag = Tag::new(*b"MINT");
    pub const NONC: Tag = Tag::new(*b"NONC");
    pub const PATH: Tag = Tag::new(*b"PATH");
    pub const PUBK: Tag = Tag::new(*b"PUBK");
    pub const RADI: Tag = Tag::new(*b"RADI");
    pub const ROOT: Tag = Tag::new(*b"ROOT");
    pub const SIG: Tag = Tag::new(*b"SIG\0");
    pub const SREP: Tag = Tag::new(*b"SREP");
    pub const SRV: Tag = Tag::new(*b"SRV\0");
    pub const TYPE: Tag = Tag::new(*b"TYPE");
    pub const VER: Tag = Tag::new(*b"VER\0");
    /// Read by no client check; the test server signs it.
    #[cfg(test)]
    pub const VERS: Tag = Tag::new(*b"VERS");
    pub const ZZZZ: Tag = Tag::new(*b"ZZZZ");

    /// The tag whose wire bytes are `bytes`.
    pub const fn new(bytes: [u8; 4]) -> Self {
        Tag(u32::from_le_bytes(bytes))
    }
}

impl<'a> Message<'a> {
    /// Parses a message.
    /// Error cases:
    /// - it is shorter than its header, or declares no pairs
    /// - an offset is not a multiple of four, decreases, or points past the end
    /// - the tags are not strictly ascending
    pub fn parse(bytes: &'a [u8]) -> anyhow::Result<Self> {
        let count = read_u32(bytes, 0).context("message too short")? as usize;
        ensure!(count > 0, "message has no pairs");
        // 4 bytes of count, 4 * (count - 1) of offsets, 4 * count of tags.
        let header_len = count.checked_mul(8).filter(|len| *len <= bytes.len()).context("message header too long")?;
        let values = &bytes[header_len..];
        let offset = |i: usize| if i == 0 { Ok(0) } else { read_u32(bytes, 4 * i).map(|o| o as usize) };
        let tag = |i: usize| read_u32(bytes, 4 * count + 4 * i).map(Tag);
        let mut pairs = Vec::with_capacity(count);
        for i in 0..count {
            let (start, end) = (offset(i)?, if i + 1 < count { offset(i + 1)? } else { values.len() });
            ensure!(start.is_multiple_of(4) && start <= end && end <= values.len(), "bad offset for value {i}");
            let tag = tag(i)?;
            if let Some((previous, _)) = pairs.last() {
                ensure!(tag > *previous, "tags out of order");
            }
            pairs.push((tag, &values[start..end]));
        }
        Ok(Self { pairs })
    }

    /// The value of `tag`.
    /// Error cases:
    /// - the message has no `tag`
    pub fn get(&self, tag: Tag) -> anyhow::Result<&'a [u8]> {
        let i = self.pairs.binary_search_by_key(&tag, |(t, _)| *t).map_err(|_| anyhow!("missing {tag:?}"))?;
        Ok(self.pairs[i].1)
    }

    /// The value of `tag`, which must be exactly `N` bytes long.
    /// Error cases:
    /// - the message has no `tag`, or its value is another length
    pub fn get_array<const N: usize>(&self, tag: Tag) -> anyhow::Result<[u8; N]> {
        let value = self.get(tag)?;
        value.try_into().with_context(|| format!("{tag:?} is {} bytes, not {N}", value.len()))
    }

    /// The value of `tag` as a little-endian uint32.
    /// Error cases:
    /// - the message has no `tag`, or its value is not four bytes
    pub fn get_u32(&self, tag: Tag) -> anyhow::Result<u32> {
        self.get_array(tag).map(u32::from_le_bytes)
    }

    /// The value of `tag` as a little-endian uint64.
    /// Error cases:
    /// - the message has no `tag`, or its value is not eight bytes
    pub fn get_u64(&self, tag: Tag) -> anyhow::Result<u64> {
        self.get_array(tag).map(u64::from_le_bytes)
    }
}

/// Encodes a message from its pairs, in any order.
pub fn encode(mut pairs: Vec<(Tag, &[u8])>) -> Vec<u8> {
    pairs.sort_by_key(|(tag, _)| *tag);
    // Callers pass at least one pair of fixed-size fields, so every value but the last is a whole
    // number of words.
    let (_, leading) = pairs.split_last().expect("a message has at least one pair");
    assert!(leading.iter().all(|(_, v)| v.len().is_multiple_of(4)), "unaligned Roughtime value");
    let mut out = Vec::new();
    out.extend_from_slice(&(pairs.len() as u32).to_le_bytes());
    let mut offset = 0;
    for (_, value) in leading {
        offset += value.len();
        out.extend_from_slice(&(offset as u32).to_le_bytes());
    }
    pairs.iter().for_each(|(tag, _)| out.extend_from_slice(&tag.0.to_le_bytes()));
    pairs.iter().for_each(|(_, value)| out.extend_from_slice(value));
    out
}

/// Frames a message as a packet.
pub fn frame(message: &[u8]) -> Vec<u8> {
    let mut packet = Vec::with_capacity(PACKET_HEADER_LEN + message.len());
    packet.extend_from_slice(PACKET_MAGIC);
    packet.extend_from_slice(&(message.len() as u32).to_le_bytes());
    packet.extend_from_slice(message);
    packet
}

/// The message inside a packet.
/// Error cases:
/// - the packet does not start with `ROUGHTIM`, or its length field does not match its size
pub fn unframe(packet: &[u8]) -> anyhow::Result<&[u8]> {
    if packet.len() < PACKET_HEADER_LEN || &packet[..8] != PACKET_MAGIC {
        bail!("not a Roughtime packet");
    }
    let message = &packet[PACKET_HEADER_LEN..];
    ensure!(read_u32(packet, 8)? as usize == message.len(), "packet length field does not match its size");
    Ok(message)
}

/// The little-endian uint32 at `at`.
/// Error cases:
/// - `bytes` ends before `at + 4`
fn read_u32(bytes: &[u8], at: usize) -> anyhow::Result<u32> {
    let word = bytes.get(at..at + 4).context("truncated")?;
    Ok(u32::from_le_bytes(word.try_into().expect("four bytes")))
}
