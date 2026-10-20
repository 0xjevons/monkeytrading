//! Collateral custody. USDC sits in a vault contract on an EVM L2. The
//! chain handles custody only and never touches execution:
//! - deposits: watch the vault's deposit events, wait for finality, then
//!   sequence `Command::Deposit`;
//! - withdrawals: the engine approves, the enclave signs, and the vault
//!   pays out against the enclave signature;
//! - checkpoints: post the signed state roots so anyone can check the
//!   enclave against the public log.

use anyhow::bail;
use tokio::sync::mpsc;

use crate::config::{CustodyProvider, CustodySection};
use crate::sequencer::SignedWithdrawal;

pub async fn run(cfg: &CustodySection, mut withdrawals: mpsc::UnboundedReceiver<SignedWithdrawal>) -> anyhow::Result<()> {
    match cfg.provider {
        CustodyProvider::Mock => {
            while let Some(w) = withdrawals.recv().await {
                tracing::info!(account = %w.account, amount = %w.amount, nonce = w.nonce, "mock custody: withdrawal signed");
            }
            Ok(())
        }
        CustodyProvider::Evm => check(cfg),
    }
}

/// Refuses to start with a provider that can't actually run.
pub fn check(cfg: &CustodySection) -> anyhow::Result<()> {
    if cfg.provider == CustodyProvider::Evm {
        bail!("EVM custody adapter not implemented yet; use custody.provider = \"mock\"");
    }
    Ok(())
}
