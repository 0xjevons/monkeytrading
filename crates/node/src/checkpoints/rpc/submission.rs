//! Crash-durable nonce ownership for one log's dedicated relayer. Host rollback or deletion of
//! this file is not detectable without sealed or remote storage; signatures authenticate only
//! the checkpoint. This store never supplies evidence of inclusion or custody finality.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use alloy::primitives::{Address, B256};
use anyhow::{Context, ensure};
use engine_core::types::AccountId;
use serde::{Deserialize, Serialize};

use crate::checkpoints::signature;
use crate::log::{Checkpoint, CheckpointDomain};

const RECORD_LIMIT: u64 = 4096;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Reservation {
    version: u8,
    domain: CheckpointDomain,
    relayer: Address,
    pub checkpoint: Checkpoint,
    pub nonce: u64,
    pub transaction_hash: Option<B256>,
}

pub(super) struct SubmissionStore {
    path: PathBuf,
    domain: CheckpointDomain,
    relayer: Address,
    signer: AccountId,
}

impl SubmissionStore {
    /// Checks existing storage without creating or changing any file.
    /// Error cases:
    /// - an existing reservation is unreadable, malformed, or unauthorized
    pub fn load(dir: &Path, domain: CheckpointDomain, relayer: Address, signer: AccountId) -> anyhow::Result<Self> {
        let store = Self { path: dir.join("checkpoint-submission.json"), domain, relayer, signer };
        store.read()?;
        Ok(store)
    }

    /// Authenticates against the caller's authority, never one taken from storage.
    /// Error cases:
    /// - the checkpoint is legacy, malformed, or signed by another authority/deployment
    pub fn validate_checkpoint(&self, checkpoint: &Checkpoint) -> anyhow::Result<()> {
        ensure!(checkpoint.parent.is_some(), "legacy checkpoint cannot reserve a v2 submission");
        signature::verify_domain(checkpoint, self.domain)?;
        ensure!(checkpoint.signer.parse::<AccountId>()? == self.signer, "submission checkpoint signer mismatch");
        Ok(())
    }

    /// Reads a bounded record and verifies its identity and checkpoint signature.
    /// Error cases:
    /// - a present file is unreadable, too large, or inconsistent with configured authority
    pub fn read(&self) -> anyhow::Result<Option<Reservation>> {
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("opening checkpoint submission reservation"),
        };
        let mut bytes = Vec::new();
        file.take(RECORD_LIMIT + 1).read_to_end(&mut bytes).context("reading checkpoint submission reservation")?;
        ensure!(bytes.len() as u64 <= RECORD_LIMIT, "checkpoint submission reservation is too large");
        let reservation: Reservation =
            serde_json::from_slice(&bytes).context("decoding checkpoint submission reservation")?;
        ensure!(reservation.version == 1, "unsupported checkpoint submission reservation version");
        ensure!(
            reservation.domain == self.domain && reservation.relayer == self.relayer,
            "checkpoint submission deployment/relayer mismatch"
        );
        ensure!(reservation.nonce < u64::MAX, "reserved relayer nonce is exhausted");
        self.validate_checkpoint(&reservation.checkpoint)?;
        Ok(Some(reservation))
    }

    /// Serializes posting instances sharing this log directory; startup never acquires it.
    /// Error cases:
    /// - the lock file cannot be opened, or another poster owns it
    pub fn lock(&self) -> anyhow::Result<File> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.path.with_extension("lock"))
            .context("opening checkpoint submission lock")?;
        file.try_lock().context("checkpoint submission lock is already held")?;
        Ok(file)
    }

    /// Stores ownership before the signing RPC can observe a transaction.
    /// Error cases:
    /// - serialization, file write/sync, rename, or directory sync fails
    pub fn reserve(&self, checkpoint: Checkpoint, nonce: u64) -> anyhow::Result<Reservation> {
        self.validate_checkpoint(&checkpoint)?;
        ensure!(nonce < u64::MAX, "relayer nonce is exhausted");
        let reservation = Reservation {
            version: 1,
            domain: self.domain,
            relayer: self.relayer,
            checkpoint,
            nonce,
            transaction_hash: None,
        };
        self.store(&reservation)?;
        Ok(reservation)
    }

    /// Atomically replaces one bounded record; the posting lock protects its single staging file.
    /// Error cases:
    /// - serialization, write/sync, rename, or directory sync fails
    pub fn store(&self, reservation: &Reservation) -> anyhow::Result<()> {
        let bytes = serde_json::to_vec(reservation).context("encoding checkpoint submission reservation")?;
        ensure!(bytes.len() as u64 <= RECORD_LIMIT, "checkpoint submission reservation is too large");
        let staging = self.path.with_extension("json.tmp");
        let mut file = File::create(&staging).context("creating checkpoint submission staging file")?;
        file.write_all(&bytes).context("writing checkpoint submission reservation")?;
        file.sync_all().context("syncing checkpoint submission reservation")?;
        fs::rename(&staging, &self.path).context("replacing checkpoint submission reservation")?;
        self.sync()
    }

    /// Reestablishes durability after a visible rename whose directory sync may have failed.
    /// Error cases:
    /// - the reservation or its containing directory cannot be synced
    pub fn sync(&self) -> anyhow::Result<()> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)
            .context("opening checkpoint submission for sync")?
            .sync_all()
            .context("syncing checkpoint submission file")?;
        let dir = self.path.parent().context("checkpoint submission has no directory")?;
        File::open(dir)
            .context("opening checkpoint submission directory")?
            .sync_all()
            .context("syncing checkpoint submission directory")
    }
}

#[cfg(test)]
mod tests;
