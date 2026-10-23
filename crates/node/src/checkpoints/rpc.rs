//! Checkpoint relaying through an operator-funded transaction RPC. The RPC cannot forge enclave
//! commitments. Reads currently trust this endpoint's canonical chain; RPC quorum is separate.

use std::future::IntoFuture;
use std::path::Path;
use std::time::Duration;

use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, Bytes};
use alloy::providers::{Provider, RootProvider};
use alloy::rpc::client::RpcClient;
use alloy::rpc::types::TransactionRequest;
use alloy::sol;
use alloy::sol_types::SolCall;
use alloy::transports::http::reqwest::{Certificate, Client, Url};
use anyhow::{Context, anyhow, ensure};
use tokio::time::timeout;

use crate::checkpoints::{CheckpointChain, VaultState, View};
use crate::config::{CheckpointSection, FinalityTag};
use crate::custody::usdc_to_fixed;
use crate::log::{Checkpoint, CheckpointDomain};

mod submission;
use crate::checkpoints::rpc::submission::SubmissionStore;

sol! {
    function checkpointSigner() external view returns (address);
    function hasCheckpoint() external view returns (bool);
    function latestSeq() external view returns (uint64);
    function latestRoot() external view returns (bytes32);
    function depositCount() external view returns (uint256);
    function usdc() external view returns (address);
    function balanceOf(address account) external view returns (uint256);
    function postCheckpoint(uint64 seq, bytes32 root, bool hasParent, uint64 parentSeq, bytes32 parentRoot, bytes signature) external;
}

pub(crate) struct CheckpointRpc {
    provider: RootProvider,
    url: Url,
    domain: CheckpointDomain,
    token: Address,
    relayer: Address,
    timeout: Duration,
    finality_tag: BlockNumberOrTag,
    confirmations: u64,
    submissions: SubmissionStore,
}

impl CheckpointRpc {
    /// Builds a client and checks existing nonce storage without requests or filesystem writes.
    /// Error cases:
    /// - the URL/certificates/client or an existing submission reservation is invalid
    pub fn new(
        cfg: &CheckpointSection,
        domain: CheckpointDomain,
        token: Address,
        finality: FinalityTag,
        confirmations: u64,
        log_dir: &Path,
        signer: engine_core::types::AccountId,
    ) -> anyhow::Result<Self> {
        let roots = webpki_root_certs::TLS_SERVER_ROOT_CERTS
            .iter()
            .map(|cert| Certificate::from_der(cert))
            .collect::<Result<Vec<_>, _>>()?;
        let http = Client::builder().tls_certs_only(roots).build()?;
        let url = cfg.rpc_url.expose()?;
        let client = RpcClient::new_http_with_client(http, url.clone());
        Ok(Self {
            provider: RootProvider::new(client),
            url,
            domain,
            token,
            relayer: cfg.relayer,
            timeout: cfg.rpc_timeout,
            finality_tag: match finality {
                FinalityTag::Latest => BlockNumberOrTag::Latest,
                FinalityTag::Safe => BlockNumberOrTag::Safe,
                FinalityTag::Finalized => BlockNumberOrTag::Finalized,
            },
            confirmations,
            submissions: SubmissionStore::load(log_dir, domain, cfg.relayer, signer)?,
        })
    }

    /// Executes a read at one canonical block hash.
    /// Error cases:
    /// - transport, timeout, EVM, or canonical-block errors
    async fn call(&self, to: Address, input: Vec<u8>, block: BlockId) -> anyhow::Result<Bytes> {
        self.deadline(self.provider.call(TransactionRequest::default().with_to(to).with_input(input)).block(block))
            .await
    }

    /// Bounds requests and redacts transport credentials from their errors.
    /// Error cases:
    /// - transport failure or timeout
    async fn deadline<T, E>(&self, future: impl IntoFuture<Output = Result<T, E>>) -> anyhow::Result<T>
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        match timeout(self.timeout, future.into_future()).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => {
                let secrets = [
                    Some(self.url.as_str()),
                    Some(self.url.path()).filter(|path| path.len() > 1),
                    self.url.query(),
                    self.url.password(),
                ];
                let message = secrets
                    .into_iter()
                    .flatten()
                    .fold(error.to_string(), |message, secret| message.replace(secret, "<redacted>"));
                Err(anyhow!("checkpoint RPC failed: {message}"))
            }
            Err(_) => Err(anyhow!("checkpoint RPC timed out")),
        }
    }
}

impl CheckpointChain for CheckpointRpc {
    /// Reads checkpoint state and collateral from one pinned canonical block.
    /// Error cases:
    /// - the endpoint is on the wrong chain or token, or an RPC/decoding error occurs
    async fn state(&self, view: View) -> anyhow::Result<VaultState> {
        let chain_id = self.deadline(self.provider.get_chain_id()).await?;
        ensure!(chain_id == self.domain.chain_id, "checkpoint endpoint is on another chain");
        let tag = if view == View::Latest { BlockNumberOrTag::Latest } else { self.finality_tag };
        let tagged = self
            .deadline(self.provider.get_block_by_number(tag))
            .await?
            .context("checkpoint RPC has no finality block")?;
        // Follow immutable parent hashes from the exact sampled tag. A by-number read could
        // switch forks between requests and give an unconfirmed replacement the old tag's depth.
        let mut block = tagged;
        if view == View::Finalized {
            ensure!(block.header.number >= self.confirmations, "checkpoint chain has insufficient confirmation depth");
            for _ in 0..self.confirmations {
                let parent = self
                    .deadline(self.provider.get_block_by_hash(block.header.parent_hash))
                    .await?
                    .context("checkpoint confirmation ancestor is missing")?;
                ensure!(
                    parent.header.hash == block.header.parent_hash
                        && parent.header.number.checked_add(1) == Some(block.header.number),
                    "checkpoint confirmation ancestry is inconsistent"
                );
                block = parent;
            }
        }
        let at = BlockId::hash_canonical(block.header.hash);
        let vault = Address::from(self.domain.vault.0);
        let token = usdcCall::abi_decode_returns(&self.call(vault, usdcCall {}.abi_encode(), at).await?)?;
        ensure!(token == self.token, "checkpoint vault holds another token");
        let signer = checkpointSignerCall::abi_decode_returns(
            &self.call(vault, checkpointSignerCall {}.abi_encode(), at).await?,
        )?;
        let has =
            hasCheckpointCall::abi_decode_returns(&self.call(vault, hasCheckpointCall {}.abi_encode(), at).await?)?;
        let checkpoint = if has {
            let seq = latestSeqCall::abi_decode_returns(&self.call(vault, latestSeqCall {}.abi_encode(), at).await?)?;
            let root =
                latestRootCall::abi_decode_returns(&self.call(vault, latestRootCall {}.abi_encode(), at).await?)?;
            Some((seq, format!("{root:#x}")))
        } else {
            None
        };
        let balance = balanceOfCall::abi_decode_returns(
            &self.call(token, balanceOfCall { account: vault }.abi_encode(), at).await?,
        )?;
        let deposits =
            depositCountCall::abi_decode_returns(&self.call(vault, depositCountCall {}.abi_encode(), at).await?)?;
        Ok(VaultState {
            height: block.header.number,
            signer: engine_core::types::AccountId(signer.into()),
            checkpoint,
            balance: usdc_to_fixed(balance).context("vault balance exceeds Fixed")?,
            deposits: u64::try_from(deposits).context("vault deposit count exceeds u64")?,
        })
    }

    /// Relays at a durable explicit nonce; only later canonical state reads confirm inclusion.
    /// Error cases:
    /// - authorization, reservation durability/ownership, or an RPC request fails
    async fn post(&self, cp: &Checkpoint) -> anyhow::Result<()> {
        self.submissions.validate_checkpoint(cp)?;
        // Serialize nonce selection and sending across posting instances for this log.
        let _lock = self.submissions.lock()?;
        let chain_id = self.deadline(self.provider.get_chain_id()).await?;
        ensure!(chain_id == self.domain.chain_id, "checkpoint endpoint is on another chain");
        let latest_nonce = self.deadline(self.provider.get_transaction_count(self.relayer)).await?;
        let mut reservation = match self.submissions.read()? {
            Some(reservation) if reservation.checkpoint == *cp => reservation,
            previous => {
                if let Some(previous) = previous {
                    ensure!(cp.seq > previous.checkpoint.seq, "checkpoint differs from its nonce reservation");
                    ensure!(latest_nonce > previous.nonce, "another checkpoint submission is still pending");
                }
                let pending_nonce = self.deadline(self.provider.get_transaction_count(self.relayer).pending()).await?;
                ensure!(
                    pending_nonce == latest_nonce,
                    "relayer has unreserved pending transactions; use a dedicated relayer"
                );
                ensure!(latest_nonce < u64::MAX, "relayer nonce is exhausted");
                self.submissions.reserve(cp.clone(), latest_nonce)?
            }
        };
        ensure!(
            latest_nonce <= reservation.nonce,
            "reserved nonce is consumed; await canonical checkpoint reconciliation"
        );
        let parent = cp.parent.context("legacy checkpoint cannot be posted to a v2 vault")?;
        let input = postCheckpointCall {
            seq: cp.seq,
            root: cp.root.parse()?,
            hasParent: parent.has_checkpoint,
            parentSeq: parent.seq,
            parentRoot: parent.root.0.into(),
            signature: cp.signature.parse()?,
        }
        .abi_encode();
        let tx = TransactionRequest::default()
            .with_to(Address::from(self.domain.vault.0))
            .with_from(self.relayer)
            .with_chain_id(self.domain.chain_id)
            .with_nonce(reservation.nonce)
            .with_input(input);
        if let Some(hash) = reservation.transaction_hash {
            let known: Option<TransactionRequest> =
                self.deadline(self.provider.client().request("eth_getTransactionByHash", (hash,))).await?;
            if let Some(known) = known {
                ensure!(
                    known.from == tx.from
                        && known.to == tx.to
                        && known.nonce == tx.nonce
                        && known.chain_id == tx.chain_id
                        && known.value.unwrap_or_default() == tx.value.unwrap_or_default()
                        && known.input.unique_input()? == tx.input.unique_input()?,
                    "reserved transaction hash identifies another transaction"
                );
                return Ok(());
            }
        }
        // A preceding sync failure must not turn a merely visible reservation into a send.
        self.submissions.sync()?;
        let hash = self.deadline(self.provider.client().request("eth_sendTransaction", (tx,))).await?;
        reservation.transaction_hash = Some(hash);
        self.submissions.store(&reservation)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
