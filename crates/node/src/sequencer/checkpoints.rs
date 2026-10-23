//! Withdrawal signature release only after a custody checkpoint commits its approval.

use anyhow::Context;
use engine_core::Fixed;
use engine_core::types::AccountId;
use tokio::sync::mpsc;
use tracing::warn;

use crate::checkpoints::{Work, liabilities};
use crate::log::{Checkpoint, LogEntry};
use crate::sequencer::{Sequencer, SignedWithdrawal};

pub(super) struct Gate {
    sink: mpsc::Sender<Work>,
    pub parent: crate::log::CheckpointParent,
    pub healthy: bool,
    pub pending: Vec<(u64, AccountId, Fixed, u64)>,
}

impl Gate {
    /// Starts paused until the first reconciliation succeeds.
    pub fn new(sink: mpsc::Sender<Work>, parent: crate::log::CheckpointParent) -> Self {
        Self { sink, parent, healthy: false, pending: Vec::new() }
    }
}

impl Sequencer {
    /// Queues a durable checkpoint; bounded backpressure stops the node instead of losing work.
    /// Error cases:
    /// - the checkpoint worker is unavailable or its bounded queue is full
    pub(super) fn queue_checkpoint(&self, checkpoint: Checkpoint) -> anyhow::Result<()> {
        let gate = self.gate.as_ref().context("checkpoint gate is absent")?;
        if gate.sink.is_closed() {
            warn!(alert = "checkpoint_post_failed", "checkpoint worker stopped; durable checkpoint awaits restart");
            return Ok(());
        }
        gate.sink
            .try_send(Work {
                checkpoint,
                liabilities: liabilities(&self.engine, self.unpaid),
                deposits: self.engine.credited_refs_with_prefix("0x").count() as u64,
            })
            .map_err(|_| anyhow::anyhow!("checkpoint worker unavailable or queue full"))
    }

    /// Releases only approvals covered by the confirmed prefix, and only after reconciliation.
    /// Error cases:
    /// - the log cannot be written
    pub(super) fn checkpoint_status(&mut self, posted: Option<u64>, healthy: bool) -> anyhow::Result<()> {
        let Some(gate) = &mut self.gate else { return Ok(()) };
        gate.healthy = healthy;
        if !healthy {
            return Ok(());
        }
        let mut ready = Vec::new();
        gate.pending.retain(|withdrawal| {
            if Some(withdrawal.0) <= posted {
                ready.push(*withdrawal);
                false
            } else {
                true
            }
        });
        for (seq, account, amount, nonce) in ready {
            let signature = self.key.sign_withdrawal(self.chain_id, &self.vault.0, account, amount, nonce);
            self.record(LogEntry::Withdrawal(SignedWithdrawal {
                account,
                amount,
                nonce,
                chain_id: self.chain_id,
                vault: self.vault,
                signature,
                approved_seq: Some(seq),
            }))?;
        }
        Ok(())
    }
}
