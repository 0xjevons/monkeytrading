//! Persisted recovery starts only after the authenticated chain anchor passes.

use tracing::error;

use crate::checkpoints::{CheckpointChain, Poster};
use crate::history::{History, HistoryPlan};
use crate::log::Checkpoint;

/// Authorizes opening or repairing history only after custody rollback and signer checks.
/// Error cases:
/// - anchor, signer, or authorized history recovery fails
pub(super) async fn finish_preflight<R: CheckpointChain>(
    plan: HistoryPlan,
    checkpoint: Option<(&Poster<R>, Option<&Checkpoint>)>,
    recovery: Option<(&crate::sequencer::ReplayedLog, &crate::sequencer::SequencerConfig<'_>)>,
) -> anyhow::Result<History> {
    if let Some((log, cfg)) = recovery {
        log.validate_recovery(cfg)?;
        if let Some((poster, _)) = checkpoint {
            log.validate_submission(cfg.checkpoint_parent, poster.chain.reserved_checkpoint()?.as_ref())?;
        }
    }
    if let Some((poster, anchor)) = checkpoint {
        poster
            .startup(anchor)
            .await
            .inspect_err(|e| error!(alert = "checkpoint_startup_failed", "refusing to serve: {e:#}"))?;
    }
    plan.finish().inspect_err(|e| error!(alert = "history_recovery_failed", "refusing to serve: {e:#}"))
}

#[cfg(test)]
mod tests;
