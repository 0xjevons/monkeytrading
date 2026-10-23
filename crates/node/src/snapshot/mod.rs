//! Signed recovery caches. Authority comes from configured attested signer pins, never the disk.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};
use engine_core::Engine;
use engine_core::eip712::{self, B256, Signature};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;
use serde::{Deserialize, Serialize};
use sha3::{Digest, Keccak256};
use tracing::{info, warn};

use crate::config::SnapshotSection;
use crate::log::{Checkpoint, Offset};
use crate::sequencer::GenesisProgress;
use crate::tee::{EnclaveKey, checkpoint_digest};

/// Replay facts carried across a checkpoint; every field is authenticated with the engine bytes.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Progress {
    pub genesis: GenesisProgress,
    pub activations: Vec<(ProtocolVersion, u64)>,
    pub checkpoints: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Snapshot {
    pub chain_id: u64,
    pub vault: AccountId,
    pub checkpoint: Checkpoint,
    pub offset: Offset,
    pub engine: String,
    pub progress: Progress,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    payload: String,
    signature: String,
}

/// Atomically publishes a synced cache after its checkpoint is synced, then prunes older caches.
/// Error cases:
/// - encoding, writing, syncing, renaming, or pruning fails, or the size limit is exceeded
pub(crate) fn write(cfg: &SnapshotSection, snapshot: &Snapshot, key: &EnclaveKey) -> anyhow::Result<()> {
    let payload = serde_json::to_string(snapshot).context("encoding snapshot")?;
    let signature = key.sign_digest(digest(snapshot.chain_id, snapshot.vault, payload.as_bytes()));
    let bytes = serde_json::to_vec(&Envelope { payload, signature }).context("encoding snapshot envelope")?;
    ensure!(bytes.len() <= cfg.max_bytes.get(), "snapshot exceeds snapshots.max_bytes");
    fs::create_dir_all(&cfg.dir).context("creating snapshot directory")?;
    let name = name(snapshot.offset);
    let staged = cfg.dir.join(format!("{name}.tmp"));
    let mut file = File::create(&staged).context("creating staged snapshot")?;
    file.write_all(&bytes).context("writing snapshot")?;
    file.sync_all().context("syncing snapshot")?;
    fs::rename(&staged, cfg.dir.join(name)).context("publishing snapshot")?;
    let dir = File::open(&cfg.dir).context("opening snapshot directory")?;
    dir.sync_all().context("syncing snapshot directory")?;
    for path in candidates(cfg)?.into_iter().skip(cfg.retain.get()) {
        fs::remove_file(path).context("pruning snapshot")?;
    }
    dir.sync_all().context("syncing snapshot retention")?;
    info!(seq = snapshot.offset.seq, "checkpoint snapshot stored");
    Ok(())
}

/// Newest cache files first; staging files are deliberately absent.
/// Error cases:
/// - an existing directory cannot be listed
pub(crate) fn candidates(cfg: &SnapshotSection) -> anyhow::Result<Vec<PathBuf>> {
    let listing = match fs::read_dir(&cfg.dir) {
        Ok(listing) => listing,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).context("listing snapshots"),
    };
    let mut paths = Vec::new();
    for entry in listing {
        let entry = entry.context("listing snapshot")?;
        let file = entry.file_name();
        if file.to_str().is_some_and(|s| {
            s.strip_suffix(".json").and_then(|s| s.split_once('-')).is_some_and(|(seq, index)| {
                seq.len() == 20 && index.len() == 10 && seq.bytes().chain(index.bytes()).all(|b| b.is_ascii_digit())
            })
        }) {
            paths.push(entry.path());
        }
    }
    paths.sort_unstable_by(|a, b| b.cmp(a));
    Ok(paths)
}

/// Authenticates the whole payload before decoding engine state, then recomputes its root.
/// Error cases:
/// - the file is unreadable, malformed, oversized, from another deployment, untrusted, or inconsistent
pub(crate) fn read(
    cfg: &SnapshotSection,
    path: &Path,
    chain_id: u64,
    vault: AccountId,
) -> anyhow::Result<(Snapshot, Engine)> {
    let mut bytes = Vec::new();
    File::open(path)?.take((cfg.max_bytes.get() as u64).saturating_add(1)).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= cfg.max_bytes.get(), "snapshot exceeds snapshots.max_bytes");
    let envelope: Envelope = serde_json::from_slice(&bytes).context("decoding snapshot envelope")?;
    let signature: Signature = envelope.signature.parse().context("snapshot signature")?;
    let signer = eip712::recover(&B256::from(digest(chain_id, vault, envelope.payload.as_bytes())), &signature)
        .context("invalid snapshot signature")?;
    ensure!(cfg.trusted_signers.contains(&signer), "snapshot signer is not independently trusted");
    let snapshot: Snapshot = serde_json::from_str(&envelope.payload).context("decoding snapshot")?;
    ensure!(snapshot.chain_id == chain_id && snapshot.vault == vault, "snapshot deployment mismatch");
    ensure!(snapshot.checkpoint.signer.parse::<AccountId>()? == signer, "snapshot checkpoint signer mismatch");
    ensure!(
        path.file_name().and_then(|s| s.to_str()) == Some(name(snapshot.offset).as_str()),
        "snapshot filename mismatch"
    );
    ensure!(snapshot.offset.seq == snapshot.checkpoint.seq, "snapshot offset mismatch");
    snapshot.progress.genesis.ensure_closed()?;
    let mut engine: Engine = serde_json::from_str(&snapshot.engine).context("decoding snapshot engine")?;
    ensure!(engine.last_seq() == Some(snapshot.checkpoint.seq), "snapshot sequence mismatch");
    let state_root = engine.state_root();
    let root = format!("0x{}", hex::encode(state_root));
    ensure!(root == snapshot.checkpoint.root, "snapshot state root mismatch");
    let cp_signature: Signature = snapshot.checkpoint.signature.parse().context("checkpoint signature")?;
    let cp_digest = checkpoint_digest(chain_id, &vault.0, snapshot.checkpoint.seq, &state_root);
    ensure!(
        eip712::recover(&B256::from(cp_digest), &cp_signature) == Some(signer),
        "invalid snapshot checkpoint signature"
    );
    Ok((snapshot, engine))
}

/// Warning episodes identify the skipped cache; another cache or full replay remains available.
pub(crate) fn rejected(path: &Path, reason: &anyhow::Error) {
    warn!(alert = "snapshot_rejected", path = %path.display(), "ignoring snapshot: {reason:#}");
}

/// Domain-separated authentication covers every byte, including replay facts and checkpoint offset.
fn digest(chain_id: u64, vault: AccountId, payload: &[u8]) -> [u8; 32] {
    let mut hash = Keccak256::new();
    hash.update(b"engine.snapshot.v1");
    hash.update(chain_id.to_be_bytes());
    hash.update(vault.0);
    hash.update(payload);
    hash.finalize().into()
}

/// Stable lexical ordering matches checkpoint order, including repeated checkpoints at one seq.
fn name(offset: Offset) -> String {
    format!("{:020}-{:010}.json", offset.seq, offset.index)
}

#[cfg(test)]
mod tests;
