//! A Roughtime client (RFC 10049): builds a request to one server and verifies its signed reply.
//!
//! It offers version 1 (RFC 10049) and 0x8000000c, the version the final drafts used
//! (draft-ietf-ntp-roughtime-12 to -19), and accepts whichever the server signs. The two differ
//! only in their number and in the capitalisation of the signature context strings. Signatures are
//! checked with `ed25519-dalek` and hashes computed with `sha2`.

use anyhow::{Context, bail, ensure};
use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest, Sha512};

mod message;
use crate::roughtime::message::{Message, Tag, encode, frame, unframe};
#[cfg(test)]
pub mod testing;

/// Roughtime version 1, from RFC 10049.
pub const VERSION_1: u32 = 1;
/// The version the final drafts used before RFC 10049 was published.
pub const VERSION_DRAFT: u32 = 0x8000_000c;
/// Offered in every request, in the ascending order the RFC requires.
const OFFERED_VERSIONS: [u32; 2] = [VERSION_1, VERSION_DRAFT];
/// A request message's size: at least 1024 bytes, so no reply is larger than its request.
const REQUEST_MESSAGE_LEN: usize = 1024;
/// The most Merkle tree levels a reply may prove its request through.
const MAX_PATH_NODES: usize = 32;
const HASH_LEN: usize = 32;

type Hash = [u8; HASH_LEN];

/// A server's signed statement of the time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reading {
    /// Seconds since the Unix epoch when the server handled the request.
    pub midpoint_s: u64,
    /// The server's bound on the error in `midpoint_s`, in seconds.
    pub radius_s: u32,
    /// The protocol version the server signed.
    pub version: u32,
}

/// One request to one server, kept to verify the reply against.
pub struct Request {
    nonce: [u8; 32],
    packet: Vec<u8>,
}

/// The context strings a version signs under, each with its terminating zero byte.
struct Contexts {
    delegation: &'static [u8],
    response: &'static [u8],
}

impl Request {
    /// A request carrying `nonce` to the server whose long-term key is `key`.
    pub fn new(key: &VerifyingKey, nonce: [u8; 32]) -> Self {
        let versions: Vec<u8> = OFFERED_VERSIONS.iter().flat_map(|v| v.to_le_bytes()).collect();
        let srv = hash(&[&[0xff], key.as_bytes()]);
        let request_type = 0u32.to_le_bytes();
        let mut fields = vec![
            (Tag::VER, versions.as_slice()),
            (Tag::SRV, srv.as_slice()),
            (Tag::NONC, nonce.as_slice()),
            (Tag::TYPE, request_type.as_slice()),
        ];
        // Header: 4 bytes of count, then an offset and a tag per further field.
        let used = 8 * (fields.len() + 1) + fields.iter().map(|(_, v)| v.len()).sum::<usize>();
        let padding = vec![0; REQUEST_MESSAGE_LEN - used];
        fields.push((Tag::ZZZZ, &padding));
        Self { nonce, packet: frame(&encode(fields)) }
    }

    /// The packet to send.
    pub fn packet(&self) -> &[u8] {
        &self.packet
    }

    /// Checks that `reply` answers this request under the server's long-term `key` (RFC 10049
    /// §5.4), and returns the time it signed.
    /// Error cases:
    /// - `reply` is not a well-formed Roughtime response, or is not a response (`TYPE` is not 1)
    /// - it answers another nonce, or signs a version this request did not offer
    /// - the delegation is not signed by `key`, or the response is not signed by the delegate
    /// - the midpoint lies outside the delegation's validity window, or the radius is zero
    /// - the Merkle path does not prove this request is in the signed tree
    pub fn verify(&self, reply: &[u8], key: &VerifyingKey) -> anyhow::Result<Reading> {
        let response = Message::parse(unframe(reply)?).context("response")?;
        ensure!(response.get_u32(Tag::TYPE)? == 1, "not a response");
        ensure!(response.get(Tag::NONC)? == self.nonce, "reply is for another request");

        let srep_bytes = response.get(Tag::SREP)?;
        let srep = Message::parse(srep_bytes).context("SREP")?;
        let version = srep.get_u32(Tag::VER)?;
        let contexts = contexts(version)?;

        let cert = Message::parse(response.get(Tag::CERT)?).context("CERT")?;
        let dele_bytes = cert.get(Tag::DELE)?;
        verify_signature(key, contexts.delegation, dele_bytes, cert.get_array(Tag::SIG)?)
            .context("delegation not signed by the server's long-term key")?;
        let dele = Message::parse(dele_bytes).context("DELE")?;
        let delegate = VerifyingKey::from_bytes(&dele.get_array(Tag::PUBK)?).context("delegated key")?;
        verify_signature(&delegate, contexts.response, srep_bytes, response.get_array(Tag::SIG)?)
            .context("response not signed by the delegated key")?;

        let midpoint_s = srep.get_u64(Tag::MIDP)?;
        let (mint, maxt) = (dele.get_u64(Tag::MINT)?, dele.get_u64(Tag::MAXT)?);
        ensure!((mint..=maxt).contains(&midpoint_s), "midpoint {midpoint_s} outside the delegation's {mint}..={maxt}");
        let radius_s = srep.get_u32(Tag::RADI)?;
        ensure!(radius_s > 0, "zero radius");
        verify_inclusion(
            &self.packet,
            response.get(Tag::PATH)?,
            response.get_u32(Tag::INDX)?,
            srep.get_array(Tag::ROOT)?,
        )?;
        Ok(Reading { midpoint_s, radius_s, version })
    }
}

/// The context strings `version` signs under.
/// Error cases:
/// - `version` is not one this client offers
fn contexts(version: u32) -> anyhow::Result<Contexts> {
    match version {
        VERSION_1 => Ok(Contexts {
            delegation: b"Roughtime v1 delegation signature\0",
            response: b"Roughtime v1 response signature\0",
        }),
        VERSION_DRAFT => Ok(Contexts {
            delegation: b"RoughTime v1 delegation signature\0",
            response: b"RoughTime v1 response signature\0",
        }),
        other => bail!("server signed version {other:#x}, which was not offered"),
    }
}

/// Checks `signature` by `key` over `context` followed by `value`.
/// Error cases:
/// - the signature does not verify (strict Ed25519 verification)
fn verify_signature(key: &VerifyingKey, context: &[u8], value: &[u8], signature: [u8; 64]) -> anyhow::Result<()> {
    let signed = [context, value].concat();
    key.verify_strict(&signed, &Signature::from_bytes(&signature)).context("bad signature")
}

/// Checks that the Merkle tree with `root` holds `request` as leaf `index`, given the sibling
/// hashes in `path` (RFC 10049 §5.3.1).
/// Error cases:
/// - `path` is not a whole number of hashes, or longer than 32 of them
/// - `index` has bits set above the path's depth
/// - the path does not lead to `root`
fn verify_inclusion(request: &[u8], path: &[u8], index: u32, root: Hash) -> anyhow::Result<()> {
    ensure!(path.len().is_multiple_of(HASH_LEN) && path.len() / HASH_LEN <= MAX_PATH_NODES, "bad Merkle path length");
    let mut bits = u64::from(index);
    let mut node = hash(&[&[0x00], request]);
    for sibling in path.as_chunks::<HASH_LEN>().0 {
        node = if bits & 1 == 0 { hash(&[&[0x01], &node, sibling]) } else { hash(&[&[0x01], sibling, &node]) };
        bits >>= 1;
    }
    ensure!(bits == 0, "Merkle index {index} is deeper than its path");
    ensure!(node == root, "request is not in the signed Merkle tree");
    Ok(())
}

/// The first 32 bytes of SHA-512 over `parts`, concatenated.
fn hash(parts: &[&[u8]]) -> Hash {
    let digest = parts.iter().fold(Sha512::new(), |hasher, part| hasher.chain_update(part)).finalize();
    digest[..HASH_LEN].try_into().expect("SHA-512 is longer than 32 bytes")
}

#[cfg(test)]
mod tests;
