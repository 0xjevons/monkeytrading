//! Test-only adaptation of immutable protocol fixtures whose signatures are placeholders.

use std::path::Path;

use engine_core::Engine;
use engine_core::auth::ClientAuth;

use crate::log::{CheckpointDomain, LogEntry, read_file};
use crate::tee::EnclaveKey;

/// Signs fixture roots for tests of historical protocol replay; never used by node recovery.
pub(crate) fn signed_fixture(path: &Path) -> Vec<LogEntry> {
    let key = EnclaveKey::generate();
    let cfg = crate::test_support::dev_config();
    let mut engine = Engine::new();
    let mut entries = Vec::new();
    read_file(path, |mut entry| {
        match &mut entry {
            LogEntry::Command(command) => {
                engine.apply(&command.sequenced)?;
            }
            LogEntry::Checkpoint(cp) => {
                let domain = engine
                    .operator()
                    .map(|operator| CheckpointDomain { chain_id: operator.chain_id, vault: operator.vault })
                    .or_else(|| match engine.client_auth() {
                        Some(ClientAuth::Eip712 { chain_id, vault }) => {
                            Some(CheckpointDomain { chain_id: *chain_id, vault: *vault })
                        }
                        _ => None,
                    })
                    .unwrap_or(CheckpointDomain {
                        chain_id: cfg.custody.chain_id,
                        vault: cfg.custody.vault.parse().unwrap(),
                    });
                cp.signature = key.sign_checkpoint(
                    domain.chain_id,
                    &domain.vault.0,
                    cp.seq,
                    &cp.root.parse::<engine_core::eip712::B256>().unwrap().0,
                );
                cp.signer = key.address_hex();
                cp.domain = Some(domain);
            }
            _ => {}
        }
        entries.push(entry);
        Ok(())
    })
    .unwrap();
    entries
}

/// Writes a signed fixture copy, leaving the committed input untouched.
pub(crate) fn copy_fixture(source: &str, target: impl AsRef<Path>) {
    let text: String = signed_fixture(source.as_ref())
        .iter()
        .map(|entry| format!("{}\n", serde_json::to_string(entry).unwrap()))
        .collect();
    std::fs::write(target, text).unwrap();
}
