//! Enclave identity and remote attestation.
//!
//! At boot the enclave generates a secp256k1 key that never leaves it. It puts the key's EVM
//! address in the attestation report data, so a hardware quote proves "this address belongs to an
//! enclave running this measurement". The vault contract and anyone reading the log trust
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
use sha3::{Digest, Keccak256};

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

#[derive(Clone, Debug, Serialize)]
pub struct Attestation {
    pub provider: String,
    pub enclave_address: String,
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

/// Attests the enclave key: its address in the first 20 bytes of the report data, the rest zero.
/// Error cases:
/// - the attestor cannot produce a quote
pub fn attest_key(attestor: &dyn Attestor, key: &EnclaveKey) -> anyhow::Result<Attestation> {
    let mut report_data = [0u8; 64];
    report_data[..20].copy_from_slice(&key.address());
    let (provider, quote) = attestor.attest(report_data)?;
    Ok(Attestation {
        provider,
        enclave_address: key.address_hex(),
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
    fn mock_attestation_binds_the_enclave_address() {
        let key = EnclaveKey::generate();
        let att = attest_key(&MockAttestor, &key).unwrap();
        assert_eq!(att.provider, "mock");
        assert!(att.report_data.starts_with(&key.address_hex()));
    }
}
