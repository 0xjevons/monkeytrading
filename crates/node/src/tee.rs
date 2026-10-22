//! Enclave identity and remote attestation.
//!
//! At boot the enclave generates a secp256k1 key that never leaves it. The attestation report data
//! carries the SHA-256 of an attestation bundle: the key's EVM address, the SHA-256 of the config
//! file, and the hash of the API's TLS public key (see `tls`). So a hardware quote proves "this
//! address belongs to an enclave running this measurement with this config, and this key
//! terminates its client connections". The vault contract and anyone reading the log trust
//! signatures from that address and nothing else. Every signed digest carries a domain tag, the
//! chain id and vault it is meant for, and a sequence number or nonce, so no signature can be
//! replayed in another context.

use std::path::PathBuf;

use anyhow::Context;
use engine_core::Fixed;
use engine_core::eip712::{self, B256};
use engine_core::types::AccountId;
use k256::ecdsa::SigningKey;
use rand::rngs::OsRng;
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use sha3::{Digest, Keccak256};

/// The attestation bundle's domain tag, which names its layout version; a new layout takes a new
/// tag.
const BUNDLE_TAG: &[u8] = b"engine.attestation.v1";

/// A source of hardware attestation quotes over caller-chosen report data.
pub trait Attestor: Send + Sync {
    /// A quote binding `report_data`, and the provider that produced it.
    /// Error cases:
    /// - the platform cannot produce a quote
    fn attest(&self, report_data: [u8; 64]) -> anyhow::Result<(String, Vec<u8>)>;
}

/// The enclave's signing key. Generated inside the enclave and never exported.
pub struct EnclaveKey {
    key: SigningKey,
}

/// What an attestation vouches for. Its encoding is the domain tag followed by each field, at fixed
/// widths and in this order; report data bytes 0..32 are the SHA-256 of that encoding, and bytes
/// 32..64 are reserved as zero.
pub struct AttestationBundle {
    /// The enclave signing key's EVM address.
    pub enclave_address: [u8; 20],
    /// SHA-256 of the config file the node loaded.
    pub config_sha256: [u8; 32],
    /// SHA-256 of the API certificate's DER SubjectPublicKeyInfo.
    pub tls_public_key_sha256: [u8; 32],
}

#[derive(Clone, Debug, Serialize)]
pub struct Attestation {
    pub provider: String,
    pub enclave_address: String,
    /// SHA-256 of the config file the node loaded, hex.
    pub config_sha256: String,
    /// SHA-256 of the API certificate's DER SubjectPublicKeyInfo, hex.
    pub tls_public_key_sha256: String,
    /// The bundle's encoding, hex: a verifier checks that its SHA-256 is report data bytes 0..32
    /// and reads the fields above from it.
    pub bundle: String,
    pub report_data: String,
    /// Hardware quote (TDX quote or SEV-SNP report), hex.
    pub quote: String,
}

/// No hardware. The "quote" says so, so nobody can mistake it for a real one.
pub struct MockAttestor;

/// Linux configfs-tsm, the kernel's common interface for TDX and SEV-SNP guests (Linux 6.7+).
/// Write the report data to `inblob`, read the quote from `outblob`.
pub struct ConfigfsTsm {
    root: PathBuf,
}

impl Attestor for MockAttestor {
    /// A fake quote: a fixed marker followed by the report data.
    /// Error cases:
    /// - none; the mock always produces a quote
    fn attest(&self, report_data: [u8; 64]) -> anyhow::Result<(String, Vec<u8>)> {
        let mut quote = b"MOCK-ATTESTATION-NOT-A-TEE:".to_vec();
        quote.extend_from_slice(&report_data);
        Ok(("mock".into(), quote))
    }
}

impl Default for ConfigfsTsm {
    /// The standard configfs mount point.
    fn default() -> Self {
        Self { root: PathBuf::from("/sys/kernel/config/tsm/report") }
    }
}

impl Attestor for ConfigfsTsm {
    /// A quote from a fresh configfs-tsm report entry, removed afterwards.
    /// Error cases:
    /// - the report directory cannot be created (not a TDX / SEV-SNP guest, or no permission)
    /// - writing `inblob`, or reading `outblob` or `provider`, fails
    fn attest(&self, report_data: [u8; 64]) -> anyhow::Result<(String, Vec<u8>)> {
        let dir = self.root.join(format!("engine-{}", std::process::id()));
        std::fs::create_dir(&dir)
            .with_context(|| format!("creating {} (is this a TDX/SEV-SNP guest?)", dir.display()))?;
        let result = (|| {
            std::fs::write(dir.join("inblob"), report_data).context("writing inblob")?;
            let quote = std::fs::read(dir.join("outblob")).context("reading outblob")?;
            let provider = std::fs::read_to_string(dir.join("provider")).context("reading provider")?;
            Ok((provider.trim().to_owned(), quote))
        })();
        let _ = std::fs::remove_dir(&dir);
        result
    }
}

impl EnclaveKey {
    /// A fresh key from the enclave's RNG.
    ///
    /// TODO: survive restarts by sealing the key to the measurement (or having a KMS release it on
    /// attestation), and hand it over between operators once rotation is on.
    pub fn generate() -> Self {
        Self { key: SigningKey::random(&mut OsRng) }
    }

    /// The key's EVM address: the last 20 bytes of keccak-256 over the uncompressed public key.
    pub fn address(&self) -> [u8; 20] {
        eip712::address(self.key.verifying_key()).0
    }

    /// The address as `0x` hex.
    pub fn address_hex(&self) -> String {
        format!("0x{}", hex::encode(self.address()))
    }

    /// Signs the state root after `seq`, for the vault on `chain_id` that will store it.
    pub fn sign_checkpoint(&self, chain_id: u64, vault: &[u8; 20], seq: u64, root: &[u8; 32]) -> String {
        self.sign_digest(checkpoint_digest(chain_id, vault, seq, root))
    }

    /// Signs an approved withdrawal for the vault on `chain_id` to pay out.
    ///
    /// TODO: move to EIP-712 typed data once the vault contract is written.
    pub fn sign_withdrawal(
        &self,
        chain_id: u64,
        vault: &[u8; 20],
        account: AccountId,
        amount: Fixed,
        nonce: u64,
    ) -> String {
        let mut h = Keccak256::new();
        h.update(b"engine.withdrawal.v1");
        h.update(chain_id.to_be_bytes());
        h.update(vault);
        h.update(account.0);
        h.update(amount.to_be_bytes());
        h.update(nonce.to_be_bytes());
        self.sign_digest(h.finalize().into())
    }

    /// 65-byte `r || s || v` with `v` in {27, 28}, as `ecrecover` expects.
    fn sign_digest(&self, digest: [u8; 32]) -> String {
        format!("0x{}", hex::encode(eip712::sign(&self.key, &B256::from(digest))))
    }
}

impl AttestationBundle {
    /// The bundle's bytes: `BUNDLE_TAG`, the enclave address, the config hash, and the TLS key hash.
    pub fn encode(&self) -> Vec<u8> {
        [BUNDLE_TAG, &self.enclave_address[..], &self.config_sha256[..], &self.tls_public_key_sha256[..]].concat()
    }

    /// The 64 bytes a quote binds: the SHA-256 of the encoding, then 32 reserved zero bytes.
    pub fn report_data(&self) -> [u8; 64] {
        let mut report_data = [0u8; 64];
        report_data[..32].copy_from_slice(&Sha256::digest(self.encode()));
        report_data
    }
}

/// Attests `bundle`: the quote binds its report data, and the attestation serves the bundle so a
/// verifier can recompute it.
/// Error cases:
/// - the attestor cannot produce a quote
pub fn attest_bundle(attestor: &dyn Attestor, bundle: &AttestationBundle) -> anyhow::Result<Attestation> {
    let report_data = bundle.report_data();
    let (provider, quote) = attestor.attest(report_data)?;
    Ok(Attestation {
        provider,
        enclave_address: format!("0x{}", hex::encode(bundle.enclave_address)),
        config_sha256: format!("0x{}", hex::encode(bundle.config_sha256)),
        tls_public_key_sha256: format!("0x{}", hex::encode(bundle.tls_public_key_sha256)),
        bundle: format!("0x{}", hex::encode(bundle.encode())),
        report_data: format!("0x{}", hex::encode(report_data)),
        quote: format!("0x{}", hex::encode(quote)),
    })
}

/// The digest a checkpoint signature covers: domain tag, chain id, vault, seq, and root.
fn checkpoint_digest(chain_id: u64, vault: &[u8; 20], seq: u64, root: &[u8; 32]) -> [u8; 32] {
    let mut h = Keccak256::new();
    h.update(b"engine.checkpoint.v1");
    h.update(chain_id.to_be_bytes());
    h.update(vault);
    h.update(seq.to_be_bytes());
    h.update(root);
    h.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};

    fn recover(digest: [u8; 32], sig_hex: &str) -> VerifyingKey {
        let sig = hex::decode(sig_hex.trim_start_matches("0x")).unwrap();
        assert_eq!(sig.len(), 65);
        let signature = Signature::from_slice(&sig[..64]).unwrap();
        let recid = RecoveryId::from_byte(sig[64] - 27).unwrap();
        VerifyingKey::recover_from_prehash(&digest, &signature, recid).unwrap()
    }

    #[test]
    fn checkpoint_signatures_recover_to_the_enclave_key() {
        let key = EnclaveKey::generate();
        let (vault, root) = ([9u8; 20], [7u8; 32]);
        let sig = key.sign_checkpoint(1, &vault, 42, &root);
        assert_eq!(&recover(checkpoint_digest(1, &vault, 42, &root), &sig), key.key.verifying_key());
    }

    #[test]
    fn checkpoint_digests_bind_the_chain_and_vault() {
        let (vault, root) = ([9u8; 20], [7u8; 32]);
        let digest = checkpoint_digest(1, &vault, 42, &root);
        assert_ne!(digest, checkpoint_digest(2, &vault, 42, &root));
        assert_ne!(digest, checkpoint_digest(1, &[8u8; 20], 42, &root));
        assert_ne!(digest, checkpoint_digest(1, &vault, 43, &root));
    }

    #[test]
    fn a_verifier_recomputes_the_report_data_from_the_served_bundle() {
        let bundle = bundle(EnclaveKey::generate().address(), [0xc1; 32], [0x7e; 32]);
        let att = attest_bundle(&MockAttestor, &bundle).unwrap();
        assert_eq!(att.provider, "mock");

        // What a verifier does: hash the served bundle, compare it with the report data, then read
        // each field from the bundle at its documented offset.
        let served = unhex(&att.bundle);
        let report_data = unhex(&att.report_data);
        assert_eq!(report_data[..32], Sha256::digest(&served)[..]);
        assert_eq!(report_data[32..], [0u8; 32]);
        assert_eq!(served.len(), 105);
        assert_eq!(&served[..21], b"engine.attestation.v1");
        assert_eq!(served[21..41], bundle.enclave_address);
        assert_eq!(served[41..73], bundle.config_sha256);
        assert_eq!(served[73..105], bundle.tls_public_key_sha256);
        // The served fields are the bundle's, and the mock quote echoes the report data it binds.
        assert_eq!(unhex(&att.enclave_address), bundle.enclave_address);
        assert_eq!(unhex(&att.config_sha256), bundle.config_sha256);
        assert_eq!(unhex(&att.tls_public_key_sha256), bundle.tls_public_key_sha256);
        assert!(att.quote.ends_with(&hex::encode(&report_data)));
    }

    #[test]
    fn changing_any_bundle_field_changes_the_report_data() {
        let (address, config, tls) = ([0x11; 20], [0x22; 32], [0x33; 32]);
        let base = bundle(address, config, tls).report_data();
        let mut flipped_address = address;
        flipped_address[19] ^= 1;
        let mut flipped_config = config;
        flipped_config[0] ^= 1;
        let mut flipped_tls = tls;
        flipped_tls[31] ^= 1;
        for (what, other) in [
            ("address", bundle(flipped_address, config, tls)),
            ("config hash", bundle(address, flipped_config, tls)),
            ("TLS key hash", bundle(address, config, flipped_tls)),
            // The fields keep their places: swapping the two hashes is another bundle.
            ("field order", bundle(address, tls, config)),
        ] {
            assert_ne!(other.report_data(), base, "a changed {what} left the report data unchanged");
            assert_eq!(other.report_data()[32..], [0u8; 32]);
        }
    }

    // ── Helpers ──

    fn bundle(
        enclave_address: [u8; 20],
        config_sha256: [u8; 32],
        tls_public_key_sha256: [u8; 32],
    ) -> AttestationBundle {
        AttestationBundle { enclave_address, config_sha256, tls_public_key_sha256 }
    }

    fn unhex(hex: &str) -> Vec<u8> {
        hex::decode(hex.trim_start_matches("0x")).unwrap()
    }
}
