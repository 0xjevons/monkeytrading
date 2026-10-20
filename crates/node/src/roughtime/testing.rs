//! A Roughtime server for tests. It signs replies with its own keys, so tests can build honest
//! and tampered responses offline.

use ed25519_dalek::{Signer, SigningKey, VerifyingKey};

use crate::roughtime::message::{Message, Tag, encode, frame, unframe};
use crate::roughtime::{VERSION_1, contexts, hash};

/// What a reply says. `TestServer::fields` fills in an honest reply; tests change fields to
/// tamper with it before signing.
pub struct ReplyFields {
    pub nonce: [u8; 32],
    pub version: u32,
    /// The version whose context strings sign the reply.
    pub signed_as: u32,
    pub midpoint_s: u64,
    pub radius_s: u32,
    pub mint: u64,
    pub maxt: u64,
    pub root: [u8; 32],
    pub path: Vec<u8>,
    pub index: u32,
    pub message_type: u32,
}

/// A server with a long-term key, and a delegated key it signs replies with.
pub struct TestServer {
    long_term: SigningKey,
    delegate: SigningKey,
    version: u32,
}

impl TestServer {
    /// A server whose keys derive from `seed`, signing replies as `version`.
    pub fn new(seed: u8, version: u32) -> Self {
        Self {
            long_term: SigningKey::from_bytes(&[seed; 32]),
            delegate: SigningKey::from_bytes(&[seed.wrapping_add(128); 32]),
            version,
        }
    }

    /// The long-term public key clients pin.
    pub fn public_key(&self) -> VerifyingKey {
        self.long_term.verifying_key()
    }

    /// The fields of an honest reply to `request`, a whole packet, at `midpoint_s` ± `radius_s`.
    pub fn fields(&self, request: &[u8], midpoint_s: u64, radius_s: u32) -> ReplyFields {
        let message = Message::parse(unframe(request).expect("request packet")).expect("request message");
        ReplyFields {
            nonce: message.get_array(Tag::NONC).expect("request nonce"),
            version: self.version,
            signed_as: self.version,
            midpoint_s,
            radius_s,
            mint: 0,
            maxt: u64::MAX,
            root: hash(&[&[0x00], request]),
            path: Vec::new(),
            index: 0,
            message_type: 1,
        }
    }

    /// An honest, signed reply to `request`.
    pub fn reply(&self, request: &[u8], midpoint_s: u64, radius_s: u32) -> Vec<u8> {
        self.sign(&self.fields(request, midpoint_s, radius_s))
    }

    /// A reply carrying `f`, signed by this server. A version the client does not know signs under
    /// version 1's context strings.
    pub fn sign(&self, f: &ReplyFields) -> Vec<u8> {
        let contexts = contexts(f.signed_as).unwrap_or_else(|_| contexts(VERSION_1).expect("version 1"));
        let (version, radius, midpoint) =
            (f.version.to_le_bytes(), f.radius_s.to_le_bytes(), f.midpoint_s.to_le_bytes());
        let srep = encode(vec![
            (Tag::VER, &version[..]),
            (Tag::RADI, &radius[..]),
            (Tag::MIDP, &midpoint[..]),
            (Tag::VERS, &version[..]),
            (Tag::ROOT, &f.root[..]),
        ]);
        let (mint, maxt) = (f.mint.to_le_bytes(), f.maxt.to_le_bytes());
        let delegate = self.delegate.verifying_key().to_bytes();
        let dele = encode(vec![(Tag::PUBK, &delegate[..]), (Tag::MINT, &mint[..]), (Tag::MAXT, &maxt[..])]);
        let dele_sig = self.long_term.sign(&[contexts.delegation, &dele].concat()).to_bytes();
        let cert = encode(vec![(Tag::SIG, &dele_sig[..]), (Tag::DELE, &dele[..])]);
        let srep_sig = self.delegate.sign(&[contexts.response, &srep].concat()).to_bytes();
        let (index, message_type) = (f.index.to_le_bytes(), f.message_type.to_le_bytes());
        frame(&encode(vec![
            (Tag::SIG, &srep_sig[..]),
            (Tag::NONC, &f.nonce[..]),
            (Tag::TYPE, &message_type[..]),
            (Tag::PATH, &f.path[..]),
            (Tag::SREP, &srep[..]),
            (Tag::CERT, &cert[..]),
            (Tag::INDX, &index[..]),
        ]))
    }
}
