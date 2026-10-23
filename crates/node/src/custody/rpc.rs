//! The vault through an EVM JSON-RPC endpoint: the chain id and the vault's token, the head and
//! the tagged block, block headers, deposit logs, and the vault's deposit count, each request
//! under a deadline, and every method a standard one.
//! Errors never carry the endpoint URL, which may hold an API key. TLS trusts only the root
//! certificates compiled into this binary, never the host-supplied store, and config validation
//! refuses plain HTTP off this machine, so the host cannot forge answers in transit.
//!
//! The quorum reader compares these answers across independent providers before the watcher
//! uses them. Configured provider independence remains an operator responsibility.

use std::future::IntoFuture;
use std::time::Duration;

use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, Bytes};
use alloy::providers::{Provider, RootProvider};
use alloy::rpc::client::RpcClient;
use alloy::rpc::types::{Filter, Header, Log, TransactionRequest};
use alloy::sol;
use alloy::sol_types::{SolCall, SolEvent};
use alloy::transports::http::reqwest::{Certificate, Client, Url};
use anyhow::{Context, anyhow, bail};
use engine_core::types::AccountId;
use tokio::time::timeout;

use crate::config::{EvmCustodySection, FinalityTag, SecretUrl};
use crate::custody::chain::BlockLink;
use crate::custody::evm::{Beneficiary, BlockRef, Finality, VaultDeposit, VaultIdentity, VaultReader};

// The vault's ABI, as `contracts/src/Vault.sol` declares it.
sol! {
    event Deposited(uint256 indexed id, address indexed account, uint256 amount);
    event HouseDeposited(uint256 indexed id, address indexed from, uint256 amount);
    function depositCount() external view returns (uint256);
    function usdc() external view returns (address);
}

pub struct VaultRpc {
    provider: RootProvider,
    /// The provider's client, for batches.
    client: RpcClient,
    /// Kept only to scrub it from errors.
    url: Url,
    vault: Address,
    finality_tag: BlockNumberOrTag,
    rpc_timeout: Duration,
}

impl VaultReader for VaultRpc {
    /// The chain id the endpoint serves and the token the vault holds; no token when nothing
    /// answers at the vault address, as on the wrong chain.
    /// Error cases:
    /// - a request fails or times out, or a non-empty answer does not decode as an address
    async fn identity(&self) -> anyhow::Result<VaultIdentity> {
        let chain_id = self.deadline("eth_chainId", self.provider.get_chain_id()).await?;
        let out = self.call(usdcCall {}.abi_encode(), BlockId::latest()).await.context("reading the vault's token")?;
        let token = if out.is_empty() {
            None
        } else {
            Some(usdcCall::abi_decode_returns(&out).context("decoding the vault's token")?)
        };
        Ok(VaultIdentity { chain_id, token })
    }

    /// The head and its timestamp, and the block at the finality tag.
    /// Error cases:
    /// - a request fails or times out, or the endpoint has no head
    async fn finality(&self) -> anyhow::Result<Finality> {
        let tagged =
            self.deadline("eth_getBlockByNumber", self.provider.get_block_by_number(self.finality_tag)).await?;
        let head = self
            .deadline("eth_getBlockByNumber", self.provider.get_block_by_number(BlockNumberOrTag::Latest))
            .await?
            .context("the RPC has no latest block")?;
        Ok(Finality {
            head: head.header.number,
            head_time: head.header.timestamp,
            tagged: tagged.map(|block| BlockRef { number: block.header.number, hash: block.header.hash }),
        })
    }

    /// The headers of blocks `from..=to`, read in one JSON-RPC batch.
    /// Error cases:
    /// - the batch fails or times out, or a block is missing or answers for another height
    async fn headers(&self, from: u64, to: u64) -> anyhow::Result<Vec<BlockLink>> {
        let mut batch = self.client.new_batch();
        let waiters = (from..=to)
            .map(|number| {
                batch.add_call::<_, Option<Header>>("eth_getBlockByNumber", &(BlockNumberOrTag::Number(number), false))
            })
            .collect::<Result<Vec<_>, _>>()
            .context("building the header batch")?;
        self.deadline("eth_getBlockByNumber batch", batch.send()).await?;
        let mut links = Vec::with_capacity(waiters.len());
        for (number, waiter) in (from..=to).zip(waiters) {
            let header = self
                .deadline("eth_getBlockByNumber", waiter)
                .await?
                .with_context(|| format!("the RPC has no block {number}"))?;
            if header.number != number {
                bail!("asked for block {number}, got block {}", header.number);
            }
            links.push(BlockLink { number, hash: header.hash, parent_hash: header.parent_hash });
        }
        Ok(links)
    }

    /// Both deposit events from the vault, sorted by block and log index.
    /// Error cases:
    /// - the request fails or times out
    /// - a log is removed, lacks its position, or does not decode as a vault deposit
    async fn deposits(&self, from: u64, to: u64) -> anyhow::Result<Vec<VaultDeposit>> {
        let filter = Filter::new()
            .address(self.vault)
            .event_signature(vec![Deposited::SIGNATURE_HASH, HouseDeposited::SIGNATURE_HASH])
            .from_block(from)
            .to_block(to);
        let logs = self.deadline("eth_getLogs", self.provider.get_logs(&filter)).await?;
        let mut deposits = logs.iter().map(|log| self.decode(log)).collect::<anyhow::Result<Vec<_>>>()?;
        deposits.sort_by_key(|d| (d.block, d.log_index));
        Ok(deposits)
    }

    /// `depositCount()` executed in block `at`, which the endpoint must hold as canonical. A node
    /// that does not have that block, holds it off its canonical chain, or has pruned its state,
    /// errors.
    /// Error cases:
    /// - the call fails or times out, or the result does not decode (see `decode_count`)
    async fn deposit_count(&self, at: BlockRef) -> anyhow::Result<u64> {
        let out =
            self.call(depositCountCall {}.abi_encode(), BlockId::hash_canonical(at.hash)).await.with_context(|| {
                format!("reading depositCount at block {}; the endpoint must serve past state", at.number)
            })?;
        decode_count(&out)
    }
}

impl VaultRpc {
    /// A client for the configured endpoint that trusts only the compiled-in TLS roots. It sends
    /// nothing until the watcher polls.
    /// Error cases:
    /// - the RPC URL is invalid, or the HTTP client cannot be built
    pub fn new(cfg: &EvmCustodySection, rpc_url: &SecretUrl, vault: Address) -> anyhow::Result<Self> {
        let url = rpc_url.expose()?;
        let roots = webpki_root_certs::TLS_SERVER_ROOT_CERTS
            .iter()
            .map(|cert| Certificate::from_der(cert))
            .collect::<Result<Vec<_>, _>>()
            .context("loading the built-in TLS roots")?;
        let http = Client::builder().tls_certs_only(roots).build().context("building the RPC client")?;
        let client = RpcClient::new_http_with_client(http, url.clone());
        Ok(VaultRpc {
            provider: RootProvider::new(client.clone()),
            client,
            url,
            vault,
            finality_tag: match cfg.finality_tag {
                FinalityTag::Latest => BlockNumberOrTag::Latest,
                FinalityTag::Safe => BlockNumberOrTag::Safe,
                FinalityTag::Finalized => BlockNumberOrTag::Finalized,
            },
            rpc_timeout: cfg.rpc_timeout,
        })
    }

    /// A read-only call to the vault at `block`.
    /// Error cases:
    /// - the call fails, reverts, or times out
    async fn call(&self, input: Vec<u8>, block: BlockId) -> anyhow::Result<Bytes> {
        let request = TransactionRequest::default().with_to(self.vault).with_input(input);
        self.deadline("eth_call", self.provider.call(request).block(block)).await
    }

    /// Awaits one RPC request under the configured timeout.
    /// Error cases:
    /// - the request times out or fails; the error text has the endpoint URL scrubbed out
    async fn deadline<T, E>(&self, method: &str, request: impl IntoFuture<Output = Result<T, E>>) -> anyhow::Result<T>
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        match timeout(self.rpc_timeout, request.into_future()).await {
            Err(_) => bail!("{method} timed out after {:?}", self.rpc_timeout),
            Ok(Err(e)) => Err(anyhow!("{method} failed: {}", self.scrub(&format!("{:#}", anyhow::Error::new(e))))),
            Ok(Ok(value)) => Ok(value),
        }
    }

    /// `text` with the endpoint URL, and its path, query, and password on their own, redacted:
    /// transport errors quote the URL, and providers put API keys in it.
    fn scrub(&self, text: &str) -> String {
        let url = &self.url;
        let secrets = [Some(url.as_str()), Some(url.path()).filter(|path| path.len() > 1), url.query(), url.password()];
        secrets.into_iter().flatten().fold(text.to_owned(), |text, secret| text.replace(secret, "<redacted>"))
    }

    /// One vault deposit from its log.
    /// Error cases:
    /// - the log is removed, from another address, lacks its block, block hash, transaction, or
    ///   index, or does not decode as either deposit event
    pub(super) fn decode(&self, log: &Log) -> anyhow::Result<VaultDeposit> {
        if log.removed || log.address() != self.vault {
            bail!("unexpected log from {} (removed: {})", log.address(), log.removed);
        }
        let block = log.block_number.context("log without a block number")?;
        let block_hash = log.block_hash.context("log without a block hash")?;
        let tx_hash = log.transaction_hash.context("log without a transaction hash")?;
        let log_index = log.log_index.context("log without a log index")?;
        let (id, beneficiary, units) = match log.topic0() {
            Some(&Deposited::SIGNATURE_HASH) => {
                let event = Deposited::decode_log(&log.inner).context("decoding Deposited")?.data;
                (event.id, Beneficiary::Trader(AccountId(event.account.into())), event.amount)
            }
            Some(&HouseDeposited::SIGNATURE_HASH) => {
                let event = HouseDeposited::decode_log(&log.inner).context("decoding HouseDeposited")?.data;
                (event.id, Beneficiary::House, event.amount)
            }
            other => bail!("unexpected event {other:?}"),
        };
        let id = u64::try_from(id).context("deposit id beyond u64")?;
        Ok(VaultDeposit { block, block_hash, tx_hash, log_index, id, beneficiary, units })
    }
}

/// The deposit count from `depositCount()`'s return data. Empty data means no code at that block:
/// the vault was deployed later, since the identity check found it at the head, so it holds none.
/// Error cases:
/// - the data does not decode, or the count exceeds `u64`
pub(super) fn decode_count(out: &Bytes) -> anyhow::Result<u64> {
    if out.is_empty() {
        return Ok(0);
    }
    let count = depositCountCall::abi_decode_returns(out).context("decoding depositCount")?;
    u64::try_from(count).context("deposit count beyond u64")
}
