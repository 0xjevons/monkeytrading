//! Enclave identity and remote attestation.
//!
//! At boot the enclave generates a secp256k1 key that never leaves it. It
//! puts the key's EVM address in the attestation report data, so a hardware
//! quote proves "this address belongs to an enclave running this
//! measurement". The vault contract and anyone reading the log trust
//! signatures from that address and nothing else.

use std::path::PathBuf;

use anyhow::Context;
use engine_core::Fixed;
use engine_core::types::AccountId;
use k256::ecdsa::SigningKey;
use serde::Serialize;
use sha3::{Digest, Keccak256};

pub struct EnclaveKey {
    key: SigningKey,
}

impl EnclaveKey {
    /// A fresh key from the enclave's RNG.
    ///
    /// TODO: survive restarts by sealing the key to the measurement (or
    /// having a KMS release it on attestation), and hand it over between
    /// operators once rotation is on.
    pub fn generate() -> Self {
        Self { key: SigningKey::random(&mut rand::rngs::OsRng) }
    }

    pub fn address(&self) -> [u8; 20] {
        let point = self.key.verifying_key().to_encoded_point(false);
        let hash = Keccak256::digest(&point.as_bytes()[1..]);
        hash[12..].try_into().expect("20 bytes")
    }

    pub fn address_hex(&self) -> String {
        format!("0x{}", hex::encode(self.address()))
    }

    pub fn sign_checkpoint(&self, seq: u64, root: &[u8; 32]) -> String {
        let mut h = Keccak256::new();
        h.update(b"engine.checkpoint.v1");
        h.update(seq.to_be_bytes());
        h.update(root);
        self.sign_digest(h.finalize().into())
    }

    /// TODO: move to EIP-712 typed data once the vault contract is written.
    pub fn sign_withdrawal(&self, chain_id: u64, vault: &[u8; 20], account: AccountId, amount: Fixed, nonce: u64) -> String {
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
        let (sig, recid) = self.key.sign_prehash_recoverable(&digest).expect("signing a 32-byte digest");
        let mut out = sig.to_bytes().to_vec();
        out.push(27 + recid.to_byte());
        format!("0x{}", hex::encode(out))
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Attestation {
    pub provider: String,
    pub enclave_address: String,
    pub report_data: String,
    /// Hardware quote (TDX quote or SEV-SNP report), hex.
    pub quote: String,
}

pub trait Attestor: Send + Sync {
    fn attest(&self, report_data: [u8; 64]) -> anyhow::Result<(String, Vec<u8>)>;
}

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

/// No hardware. The "quote" says so, so nobody can mistake it for a real one.
pub struct MockAttestor;

impl Attestor for MockAttestor {
    fn attest(&self, report_data: [u8; 64]) -> anyhow::Result<(String, Vec<u8>)> {
        let mut quote = b"MOCK-ATTESTATION-NOT-A-TEE:".to_vec();
        quote.extend_from_slice(&report_data);
        Ok(("mock".into(), quote))
    }
}

/// Linux configfs-tsm, the kernel's common interface for TDX and SEV-SNP
/// guests (Linux 6.7+). Write the report data to `inblob`, read the quote
/// from `outblob`.
pub struct ConfigfsTsm {
    root: PathBuf,
}

impl Default for ConfigfsTsm {
    fn default() -> Self {
        Self { root: PathBuf::from("/sys/kernel/config/tsm/report") }
    }
}

impl Attestor for ConfigfsTsm {
    fn attest(&self, report_data: [u8; 64]) -> anyhow::Result<(String, Vec<u8>)> {
        let dir = self.root.join(format!("engine-{}", std::process::id()));
        std::fs::create_dir(&dir).with_context(|| format!("creating {} (is this a TDX/SEV-SNP guest?)", dir.display()))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};

    #[test]
    fn checkpoint_signatures_recover_to_the_enclave_address() {
        let key = EnclaveKey::generate();
        let root = [7u8; 32];
        let sig = hex::decode(key.sign_checkpoint(42, &root).trim_start_matches("0x")).unwrap();
        assert_eq!(sig.len(), 65);

        let mut h = Keccak256::new();
        h.update(b"engine.checkpoint.v1");
        h.update(42u64.to_be_bytes());
        h.update(root);
        let digest: [u8; 32] = h.finalize().into();
        let recovered = VerifyingKey::recover_from_prehash(
            &digest,
            &Signature::from_slice(&sig[..64]).unwrap(),
            RecoveryId::from_byte(sig[64] - 27).unwrap(),
        )
        .unwrap();
        assert_eq!(&recovered, key.key.verifying_key());
    }

    #[test]
    fn mock_attestation_binds_the_enclave_address() {
        let key = EnclaveKey::generate();
        let att = attest_key(&MockAttestor, &key).unwrap();
        assert_eq!(att.provider, "mock");
        assert!(att.report_data.starts_with(&key.address_hex()));
    }
}
