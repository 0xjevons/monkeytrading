//! Snapshot-assisted recovery leaves public audit replay on the full log path.

use std::path::Path;

use anyhow::{Context, ensure};
use engine_core::types::AccountId;
use tracing::{info, warn};

use crate::config::SnapshotSection;
use crate::log::{Checkpoint, LogEntry, SegmentedLog, offset_of};
use crate::sequencer::replay::Replay;
use crate::sequencer::{ReplayedLog, Sequencer};
use crate::snapshot::{self, Snapshot};

impl Sequencer {
    /// Tries authenticated caches newest first, then replays from genesis. An invalid cache is
    /// optional data and never makes a verified log unavailable.
    /// Error cases:
    /// - the log cannot be read or fails full replay when no cache succeeds
    pub(crate) fn read_log_with_snapshots(
        dir: &Path,
        cfg: Option<&SnapshotSection>,
        chain_id: u64,
        vault: AccountId,
    ) -> anyhow::Result<ReplayedLog> {
        if let Some(cfg) = cfg {
            match snapshot::candidates(cfg) {
                Ok(paths) => {
                    for path in paths {
                        let recovered = snapshot::read(cfg, &path, chain_id, vault).and_then(|(snapshot, engine)| {
                            let (at, checkpoint) = (snapshot.offset, snapshot.checkpoint.clone());
                            let mut replay = Replay::from_snapshot(engine, snapshot);
                            let (mut offset, mut found) = (None, false);
                            let log = SegmentedLog::open_after(dir, Some(at), |entry| {
                                let next = offset_of(offset, &entry)?;
                                offset = Some(next);
                                if next == at {
                                    ensure!(
                                        matches!(&entry, LogEntry::Checkpoint(cp) if *cp == checkpoint),
                                        "snapshot checkpoint differs from log"
                                    );
                                    found = true;
                                } else if next > at {
                                    ensure!(found, "snapshot checkpoint missing from log");
                                    replay.apply(&entry)?;
                                }
                                Ok(())
                            })?;
                            ensure!(found, "snapshot checkpoint missing from log");
                            let replayed = replay.finish();
                            info!(
                                snapshot_seq = at.seq,
                                replayed_commands = replayed.commands - at.seq - 1,
                                "recovered from checkpoint snapshot"
                            );
                            Ok(ReplayedLog { log, replayed, snapshot_seq: Some(at.seq) })
                        });
                        match recovered {
                            Ok(log) => return Ok(log),
                            Err(reason) => snapshot::rejected(&path, &reason),
                        }
                    }
                }
                Err(reason) => snapshot::rejected(&cfg.dir, &reason),
            }
        }
        Self::read_log(dir, |_| {})
    }

    /// Syncs the authoritative checkpoint before publishing a disposable snapshot. Failed cache
    /// writes warn and leave the log usable; a failed log sync fails closed.
    /// Error cases:
    /// - the authoritative log cannot be synced
    pub(super) fn store_snapshot(&mut self, checkpoint: Checkpoint) -> anyhow::Result<()> {
        let Some(cfg) = &self.snapshots else {
            return Ok(());
        };
        if self.progress.genesis.ensure_closed().is_err() {
            return Ok(());
        }
        self.log.sync()?;
        let write = || {
            let offset = self.outbox.last().context("snapshot without checkpoint entry")?.offset;
            let snapshot = Snapshot {
                chain_id: self.chain_id,
                vault: self.vault,
                checkpoint,
                offset,
                engine: serde_json::to_string(&self.engine).context("encoding snapshot engine")?,
                progress: self.progress.clone(),
            };
            snapshot::write(cfg, &snapshot, &self.key)
        };
        if let Err(reason) = write() {
            warn!(alert = "snapshot_write_failed", dir = %cfg.dir.display(), "checkpoint remains in log: {reason:#}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
