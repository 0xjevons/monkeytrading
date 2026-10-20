//! Collateral custody. USDC sits in a vault contract on an EVM L2. The chain handles custody only
//! and never touches execution:
//! - deposits: watch the vault's deposit events, wait for finality, then sequence
//!   `Command::Deposit` with the transaction hash and log index as its reference;
//! - withdrawals: the engine approves, the enclave signs, and the vault pays out against the
//!   enclave signature;
//! - checkpoints: post the signed state roots so anyone can check the enclave against the public
//!   log.

use anyhow::bail;
use tokio::sync::mpsc;
use tracing::info;

use crate::config::{CustodyProvider, CustodySection};
use crate::sequencer::SignedWithdrawal;

/// Runs the custody adapter until the sequencer stops sending withdrawals.
/// Error cases:
/// - the configured provider is not implemented
pub async fn run(
    cfg: &CustodySection,
    mut withdrawals: mpsc::UnboundedReceiver<SignedWithdrawal>,
) -> anyhow::Result<()> {
    check(cfg)?;
    while let Some(w) = withdrawals.recv().await {
        info!(account = %w.account, amount = %w.amount, nonce = w.nonce, "mock custody: withdrawal signed");
    }
    Ok(())
}

/// Refuses to start with a provider that can't actually run.
/// Error cases:
/// - `CustodyProvider::Evm`, which is not implemented yet
pub fn check(cfg: &CustodySection) -> anyhow::Result<()> {
    match cfg.provider {
        CustodyProvider::Mock => Ok(()),
        CustodyProvider::Evm => bail!("EVM custody adapter not implemented yet; use custody.provider = \"mock\""),
    }
}
