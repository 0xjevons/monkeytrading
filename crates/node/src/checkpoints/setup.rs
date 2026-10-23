//! Checkpoint setup before the node opens its log for writing.

use std::sync::Arc;

use anyhow::Context;
use engine_core::types::AccountId;
use tokio::sync::mpsc;

use crate::checkpoints::{CheckpointChain, CheckpointRpc, Poster, Work};
use crate::config::NodeConfig;
use crate::log::CheckpointDomain;
use crate::tee::EnclaveKey;

pub(crate) struct Setup {
    pub poster: Poster<CheckpointRpc>,
    pub tx: mpsc::Sender<Work>,
    pub rx: mpsc::Receiver<Work>,
    pub parent: crate::log::CheckpointParent,
    pub posted_seq: Option<u64>,
}

/// Connects the checkpoint endpoint and reads its rollback anchor before log recovery.
/// Error cases:
/// - configuration, transport, or vault reads fail
pub(crate) async fn connect(cfg: &NodeConfig, key: &Arc<EnclaveKey>) -> anyhow::Result<Option<Setup>> {
    let Some(checkpoints) = &cfg.checkpoints else { return Ok(None) };
    let domain = CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse()? };
    let token = cfg.custody.evm.as_ref().context("checkpoint custody is not EVM")?.usdc;
    let chain = CheckpointRpc::new(checkpoints, domain, token)?;
    let state = chain.state().await?;
    let parent = match state.checkpoint {
        Some((seq, root)) => crate::log::CheckpointParent { has_checkpoint: true, seq, root: root.parse()? },
        None => Default::default(),
    };
    let posted_seq = parent.has_checkpoint.then_some(parent.seq);
    let poster = Poster {
        chain,
        domain,
        signer: AccountId(key.address()),
        retry: checkpoints.retry_interval,
        pending_deposits_critical_after: cfg
            .custody
            .evm
            .as_ref()
            .context("checkpoint custody is not EVM")?
            .stall_critical_after,
    };
    let (tx, rx) = mpsc::channel(64);
    Ok(Some(Setup { poster, tx, rx, posted_seq, parent }))
}
