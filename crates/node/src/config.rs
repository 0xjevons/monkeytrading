//! Node configuration: one TOML file, loaded and validated once at startup. The `[genesis]`
//! section is protocol and is sequenced into the log on first boot; everything else configures
//! this node only.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use engine_core::Fixed;
use engine_core::market::MarketSpec;
use engine_core::params::ProtocolParams;
use serde::Deserialize;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    /// Trusts the `account` field on requests. Local development only.
    None,
    /// Requests must carry an EIP-712 signature from the account's key.
    Eip712,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeeProvider {
    /// No hardware; attestation reports are clearly marked as fake.
    Mock,
    /// Linux configfs-tsm (`/sys/kernel/config/tsm/report`), which serves both Intel TDX and AMD
    /// SEV-SNP confidential VMs.
    ConfigfsTsm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CustodyProvider {
    /// No chain. Exposes a dev deposit endpoint and logs withdrawals.
    Mock,
    /// Watches vault deposits on an EVM chain and submits signed withdrawals.
    Evm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedProvider {
    Mock,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    pub node: NodeSection,
    pub logging: LoggingSection,
    pub tee: TeeSection,
    pub operator: OperatorSection,
    pub custody: CustodySection,
    pub feeds: FeedsSection,
    /// Sequenced as admin commands when the log is empty, so the rules every fill ran under are
    /// part of the public record.
    pub genesis: Genesis,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeSection {
    pub listen: SocketAddr,
    /// The log's segment files and `manifest.json`. Completed segments and the manifest are the
    /// public archive, served at `/v1/archive/`; every segment is kept, since recovery replays the
    /// log from genesis.
    pub log_dir: PathBuf,
    /// Start a new log segment once the current one holds this many commands.
    pub segment_commands: u64,
    /// Write a signed state root every N commands.
    pub checkpoint_every: u64,
    /// Interval of `Tick` commands that drive sessions and rollovers.
    #[serde(with = "humantime_serde")]
    pub tick: Duration,
    pub auth: AuthMode,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoggingSection {
    /// A `tracing` filter directive, e.g. `"info"` or `"info,engine_node=debug"`.
    pub level: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeeSection {
    pub provider: TeeProvider,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorSection {
    /// Operator rotation is off at launch: one operator runs the enclave while the product
    /// settles. When it's switched on, operators hand over by passing the log and the sealed
    /// signing key between attested enclaves.
    pub rotation_enabled: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustodySection {
    pub provider: CustodyProvider,
    pub chain_id: u64,
    /// The vault contract that holds collateral and honours enclave-signed withdrawals.
    pub vault: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedsSection {
    pub provider: FeedProvider,
    #[serde(with = "humantime_serde")]
    pub interval: Duration,
    /// Starting mids for the mock feed, by market id.
    #[serde(default)]
    pub mock_mids: Vec<MockMid>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MockMid {
    pub market: u32,
    pub mid: Fixed,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Genesis {
    pub house_capital: Fixed,
    pub params: ProtocolParams,
    #[serde(default)]
    pub benchmarks: Vec<Benchmark>,
    pub markets: Vec<MarketSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Benchmark {
    pub name: String,
    pub annual_bps: i64,
}

impl NodeConfig {
    /// Reads and validates the config file at `path`.
    /// Error cases:
    /// - the file cannot be read, or is not valid TOML for this schema (unknown keys included)
    /// - `logging.level` is not a valid filter directive
    /// - `node.tick`, `node.checkpoint_every`, `node.segment_commands`, or `feeds.interval` is zero
    /// - genesis parameters or a market spec fail validation
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let cfg: NodeConfig = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.validate().with_context(|| format!("validating {}", path.display()))?;
        Ok(cfg)
    }

    /// Checks values that parse but would fail silently or at the wrong moment.
    /// Error cases:
    /// - `logging.level` is not a valid filter directive
    /// - `node.tick`, `node.checkpoint_every`, `node.segment_commands`, or `feeds.interval` is zero
    /// - genesis parameters or a market spec fail validation
    fn validate(&self) -> anyhow::Result<()> {
        EnvFilter::try_new(&self.logging.level).context("logging.level")?;
        if self.node.tick.is_zero()
            || self.node.checkpoint_every == 0
            || self.node.segment_commands == 0
            || self.feeds.interval.is_zero()
        {
            anyhow::bail!(
                "node.tick, node.checkpoint_every, node.segment_commands, and feeds.interval must be non-zero"
            );
        }
        self.genesis.params.validate().map_err(anyhow::Error::msg).context("genesis.params")?;
        for m in &self.genesis.markets {
            m.validate().map_err(anyhow::Error::msg).with_context(|| format!("market {}", m.symbol))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEV: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/dev.toml");

    #[test]
    fn the_committed_dev_config_loads() {
        let cfg = NodeConfig::load(Path::new(DEV)).unwrap();
        assert_eq!(cfg.node.tick, Duration::from_millis(250));
        assert!(!cfg.genesis.markets.is_empty());
        assert!(!cfg.operator.rotation_enabled);
    }

    #[test]
    fn a_mistyped_key_fails_the_boot() {
        let text = std::fs::read_to_string(DEV).unwrap().replace("checkpoint_every", "checkpoint_evry");
        assert!(toml::from_str::<NodeConfig>(&text).is_err());
        let text = std::fs::read_to_string(DEV).unwrap().replace("house_cap_bps", "house_capp_bps");
        assert!(toml::from_str::<NodeConfig>(&text).is_err());
    }

    #[test]
    fn a_zero_segment_size_is_refused() {
        let text = std::fs::read_to_string(DEV).unwrap().replace("segment_commands = 1000", "segment_commands = 0");
        let cfg: NodeConfig = toml::from_str(&text).unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn a_zero_tick_is_refused() {
        let text = std::fs::read_to_string(DEV).unwrap().replace("tick = \"250ms\"", "tick = \"0s\"");
        let cfg: NodeConfig = toml::from_str(&text).unwrap();
        assert!(cfg.validate().is_err());
    }
}
