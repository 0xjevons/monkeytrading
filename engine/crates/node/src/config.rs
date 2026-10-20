use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::Context;
use engine_core::Fixed;
use engine_core::market::MarketSpec;
use engine_core::params::ProtocolParams;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct NodeConfig {
    pub node: NodeSection,
    pub tee: TeeSection,
    pub operator: OperatorSection,
    pub custody: CustodySection,
    pub feeds: FeedsSection,
    /// Sequenced as admin commands when the log is empty, so the rules every
    /// fill ran under are part of the public record.
    pub genesis: Genesis,
}

#[derive(Debug, Deserialize)]
pub struct NodeSection {
    pub listen: SocketAddr,
    pub log_path: PathBuf,
    /// Write a signed state root every N commands.
    pub checkpoint_every: u64,
    /// Interval of `Tick` commands that drive sessions and rollovers.
    pub tick_ms: u64,
    pub auth: AuthMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    /// Trusts the `account` field on requests. Local development only.
    None,
    /// Requests must carry an EIP-712 signature from the account's key.
    Eip712,
}

#[derive(Debug, Deserialize)]
pub struct TeeSection {
    pub provider: TeeProvider,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeeProvider {
    /// No hardware; attestation reports are clearly marked as fake.
    Mock,
    /// Linux configfs-tsm (`/sys/kernel/config/tsm/report`), which serves
    /// both Intel TDX and AMD SEV-SNP confidential VMs.
    ConfigfsTsm,
}

#[derive(Debug, Deserialize)]
pub struct OperatorSection {
    /// Operator rotation is off at launch: one operator runs the enclave
    /// while the product settles. When it's switched on, operators hand over
    /// by passing the log and the sealed signing key between attested enclaves.
    pub rotation_enabled: bool,
}

#[derive(Debug, Deserialize)]
pub struct CustodySection {
    pub provider: CustodyProvider,
    pub chain_id: u64,
    /// The vault contract that holds collateral and honours enclave-signed withdrawals.
    pub vault: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CustodyProvider {
    /// No chain. Exposes a dev deposit endpoint and logs withdrawals.
    Mock,
    /// Watches vault deposits on an EVM chain and submits signed withdrawals.
    Evm,
}

#[derive(Debug, Deserialize)]
pub struct FeedsSection {
    pub provider: FeedProvider,
    pub interval_ms: u64,
    /// Starting mids for the mock feed, by market id.
    #[serde(default)]
    pub mock_mids: Vec<MockMid>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedProvider {
    Mock,
}

#[derive(Debug, Deserialize)]
pub struct MockMid {
    pub market: u32,
    pub mid: Fixed,
}

#[derive(Debug, Deserialize)]
pub struct Genesis {
    pub house_capital: Fixed,
    pub params: ProtocolParams,
    #[serde(default)]
    pub benchmarks: Vec<Benchmark>,
    pub markets: Vec<MarketSpec>,
}

#[derive(Debug, Deserialize)]
pub struct Benchmark {
    pub name: String,
    pub annual_bps: i64,
}

impl NodeConfig {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let cfg: NodeConfig = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.genesis.params.validate().map_err(anyhow::Error::msg).context("genesis.params")?;
        for m in &cfg.genesis.markets {
            m.validate().map_err(anyhow::Error::msg).with_context(|| format!("market {}", m.symbol))?;
        }
        Ok(cfg)
    }
}
