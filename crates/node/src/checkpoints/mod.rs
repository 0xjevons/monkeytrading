//! On-chain checkpoint commitments, replay protection, and collateral reconciliation.

mod rpc;
pub(crate) mod setup;
pub(crate) mod signature;
pub(crate) use rpc::CheckpointRpc;

use std::collections::VecDeque;
use std::time::Duration;

use anyhow::{Context, ensure};
use engine_core::{Engine, Fixed};
use tokio::sync::mpsc;
use tokio::time::{MissedTickBehavior, interval};
use tracing::{error, info, warn};

use crate::log::{Checkpoint, CheckpointDomain};
use crate::sequencer::Request;

/// A signed checkpoint and the custody liabilities its replayed state carries.
#[derive(Clone, Debug)]
pub(crate) struct Work {
    pub checkpoint: Checkpoint,
    pub liabilities: Fixed,
    pub deposits: u64,
}

/// The latest commitment and collateral, read at one canonical block hash.
#[derive(Clone, Debug)]
pub(crate) struct VaultState {
    pub signer: engine_core::types::AccountId,
    pub checkpoint: Option<(u64, String)>,
    pub balance: Fixed,
    pub deposits: u64,
}

/// Chain access, kept separate from the authorized enclave signer lifecycle.
pub(crate) trait CheckpointChain: Send + Sync {
    /// Reads the authorized signer, latest checkpoint, and collateral from one block.
    /// Error cases:
    /// - an RPC call fails, decodes incorrectly, or serves another chain
    async fn state(&self) -> anyhow::Result<VaultState>;
    /// Relays a signed checkpoint. Receipt loss is safe: the next state read detects the post.
    /// Error cases:
    /// - submission fails, times out, or reverts
    async fn post(&self, checkpoint: &Checkpoint) -> anyhow::Result<()>;
}

#[derive(Debug)]
struct Conflict(&'static str);

impl std::fmt::Display for Conflict {
    /// The commitment invariant that failed.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for Conflict {}

/// Supervised checkpoint worker; work is durable in the log before it reaches this queue.
pub(crate) struct Poster<R> {
    pub chain: R,
    pub domain: CheckpointDomain,
    pub signer: engine_core::types::AccountId,
    pub retry: Duration,
    pub pending_deposits_critical_after: Duration,
}

impl<R: CheckpointChain> Poster<R> {
    /// Checks the chain anchor before recovery opens the log for writing or binds any listener.
    /// Error cases:
    /// - reading fails, the authorized signer differs, or the log does not contain the posted root
    pub async fn startup(&self, anchor: Option<&Checkpoint>) -> anyhow::Result<()> {
        let state = self.chain.state().await?;
        ensure!(state.signer == self.signer, "vault signer differs from this enclave; signer recovery is required");
        if let Some((seq, root)) = state.checkpoint {
            let cp = anchor.context("log is shorter than the latest posted checkpoint")?;
            ensure!(cp.seq == seq && cp.root == root, "log differs from the latest posted checkpoint at seq {seq}");
            signature::verify_domain(cp, self.domain)?;
            ensure!(
                cp.signer.parse::<engine_core::types::AccountId>()? == state.signer,
                "posted checkpoint signer mismatch"
            );
        }
        Ok(())
    }

    /// Posts checkpoints in log order, retries transport failures, and reports withdrawal health.
    /// Error cases:
    /// - the sequencer disappears, or the chain exposes an incompatible checkpoint/signer
    pub async fn run(self, mut rx: mpsc::Receiver<Work>, tx: mpsc::Sender<Request>) -> anyhow::Result<()> {
        let mut queued = VecDeque::new();
        let mut latest = None;
        let mut pending_since = None;
        let mut timer = interval(self.retry);
        timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                work = rx.recv(), if queued.len() < 64 => match work {
                    Some(work) => queued.push_back(work),
                    None => return Ok(()),
                },
                _ = timer.tick() => {
                    let work = queued.front().or(latest.as_ref());
                    let Some(work) = work else { continue };
                    let result = self.reconcile(work).await;
                    if matches!(result, Ok((Some(_), false))) {
                        let since = pending_since.get_or_insert_with(tokio::time::Instant::now);
                        if since.elapsed() >= self.pending_deposits_critical_after {
                            error!(alert = "collateral_mismatch", "withdrawals paused: collateral reconciliation exceeded its catch-up deadline");
                        }
                    } else if matches!(result, Ok((_, true))) {
                        pending_since = None;
                    }
                    let (posted, healthy) = match result {
                        Ok(status) => status,
                        Err(error) => {
                            if error.is::<Conflict>() {
                                error!(alert = "checkpoint_conflict", "refusing to continue: {error:#}");
                                return Err(error);
                            }
                            warn!(alert = "checkpoint_post_failed", "checkpoint remains queued: {error:#}");
                            (None, false)
                        }
                    };
                    tx.send(Request::CheckpointStatus { posted, healthy }).await.map_err(|_| anyhow::anyhow!("sequencer stopped"))?;
                    if posted.is_some() && !queued.is_empty() {
                        latest = queued.pop_front();
                    }
                }
            }
        }
    }

    /// Verifies/extends the commitment and compares collateral to the checkpoint's ledger.
    /// Error cases:
    /// - the chain read or post fails, or a signer/root conflicts with the local log
    async fn reconcile(&self, work: &Work) -> anyhow::Result<(Option<u64>, bool)> {
        let cp = &work.checkpoint;
        signature::verify_domain(cp, self.domain)?;
        let mut state = self.chain.state().await?;
        if state.signer != self.signer || cp.signer.parse::<engine_core::types::AccountId>()? != self.signer {
            return Err(Conflict("checkpoint signer differs from the authorized vault signer").into());
        }
        match &state.checkpoint {
            Some((seq, root)) if *seq == cp.seq => {
                if *root != cp.root {
                    return Err(Conflict("conflicting posted checkpoint root").into());
                }
            }
            Some((seq, _)) if *seq > cp.seq => {
                return Err(Conflict("vault advanced beyond the local checkpoint").into());
            }
            _ => {
                let parent = cp.parent.context("on-chain checkpoints require signed parent metadata")?;
                let expected = state
                    .checkpoint
                    .as_ref()
                    .map(|(seq, root)| -> anyhow::Result<_> {
                        Ok(crate::log::CheckpointParent {
                            has_checkpoint: true,
                            seq: *seq,
                            root: root.parse::<engine_core::eip712::B256>()?,
                        })
                    })
                    .transpose()?
                    .unwrap_or_default();
                if parent != expected {
                    return Err(Conflict("signed parent differs from the latest vault checkpoint").into());
                }
                self.chain.post(cp).await?;
                // A submitted transaction is not a commitment. Verify inclusion before release.
                state = self.chain.state().await?;
                ensure!(
                    state.checkpoint == Some((cp.seq, cp.root.clone())) && state.signer == self.signer,
                    "checkpoint transaction is not included in the authorized vault"
                );
            }
        }
        let healthy = state.balance == work.liabilities && state.deposits == work.deposits;
        if healthy {
            info!(alert_resolved = "collateral_mismatch", "vault collateral reconciles");
            info!(alert_resolved = "checkpoint_post_failed", "checkpoint is posted");
            info!(alert_resolved = "collateral_pending_deposits", "vault deposits are credited");
        } else if state.deposits > work.deposits && state.balance >= work.liabilities {
            warn!(
                alert = "collateral_pending_deposits",
                "withdrawals await the checkpoint covering final deposit credits"
            );
        } else {
            error!(
                alert = "collateral_mismatch",
                "withdrawals paused: vault collateral or deposit count differs from ledger"
            );
        }
        Ok((Some(cp.seq), healthy))
    }
}

/// Ledger cash plus approvals not paid on chain. Payout settlement will reduce `unpaid`.
pub(crate) fn liabilities(engine: &Engine, unpaid: Fixed) -> Fixed {
    engine.accounts().values().fold(engine.house_cash() + unpaid, |total, account| total + account.balance)
}

#[cfg(test)]
mod scenario;
#[cfg(test)]
mod tests;
