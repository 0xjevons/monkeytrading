//! Node configuration: one TOML file, loaded and validated once at startup. Two parts are protocol
//! and go into the log: the client auth (`node.auth`, with `custody.chain_id` and `custody.vault`
//! under `eip712`), recorded when the log first activates protocol versions and fixed for the life
//! of the log; and the `[genesis]` section, sequenced after protocol activation. Everything else
//! configures this node only.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use engine_core::Fixed;
use engine_core::auth::ClientAuth;
use engine_core::market::MarketSpec;
use engine_core::params::{MAX_BENCHMARK_BPS, ProtocolParams};
use engine_core::types::{AccountId, MarketId};
use serde::Deserialize;
use tokio_tungstenite::tungstenite::http::Uri;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    /// Trusts the `account` field on requests. Local development only: it needs the mock TEE and
    /// mock custody.
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
    /// A random walk quoted by three fake venues. Local development only.
    Mock,
    /// Live venue WebSockets from `[feeds.venues]`, one supervised task per connection.
    Venues,
}

/// A venue whose public WebSocket feed the node reads. Its name is the quote source in the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Venue {
    Bitstamp,
    Coinbase,
    Kraken,
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
    /// Sequenced as admin commands after protocol activation, and again on each boot until its
    /// house capital credit is in the log, so the rules every fill ran under are part of the public
    /// record.
    pub genesis: Genesis,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeSection {
    pub listen: SocketAddr,
    /// The log's segment files and `manifest.jsonl`. Completed segments and the manifest are the
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
    /// The mock feed's tick, and the most often a venue feed sequences one market's latest quote.
    #[serde(with = "humantime_serde")]
    pub interval: Duration,
    /// Starting mids for the mock feed, by market id.
    #[serde(default)]
    pub mock_mids: Vec<MockMid>,
    /// Required when `provider = "venues"`; validated whenever present.
    pub venues: Option<VenuesSection>,
}

/// Live venue feeds.
///
/// TODO: this file comes from host storage, so the host chooses the venues and the symbol each
/// market reads; the attestation must bind the config's hash before that choice is verifiable.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VenuesSection {
    /// Deadline to connect and subscribe, and the longest a connection, or one of its symbols, may
    /// go without a fresh quote before the connection is replaced and the gap alerted.
    #[serde(with = "humantime_serde")]
    pub timeout: Duration,
    /// The first reconnect delay; it doubles after each attempt that sequences no fresh quote.
    #[serde(with = "humantime_serde")]
    pub reconnect_min: Duration,
    #[serde(with = "humantime_serde")]
    pub reconnect_max: Duration,
    /// A quote older than this when sequenced does not count as its venue quoting: it ends no
    /// outage or silence. Keep it at or below the markets' `max_age_ms`.
    #[serde(with = "humantime_serde")]
    pub stale_after: Duration,
    pub connections: Vec<VenueConnection>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VenueConnection {
    pub venue: Venue,
    /// The venue's public endpoint: `wss://` on the venue's own domain. The host controls the
    /// network, so plaintext would let it forge prices, and TLS alone proves only the server's
    /// name, not that it is the venue the quotes are labelled with.
    pub url: String,
    pub symbols: Vec<VenueSymbol>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VenueSymbol {
    pub market: MarketId,
    /// The venue's own name for the market's instrument, e.g. `BTC-USD`.
    pub symbol: String,
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

impl Venue {
    /// The source name this venue's quotes carry.
    pub fn name(self) -> &'static str {
        match self {
            Venue::Bitstamp => "bitstamp",
            Venue::Coinbase => "coinbase",
            Venue::Kraken => "kraken",
        }
    }

    /// The domain this venue's endpoints are served under.
    fn domain(self) -> &'static str {
        match self {
            Venue::Bitstamp => "bitstamp.net",
            Venue::Coinbase => "coinbase.com",
            Venue::Kraken => "kraken.com",
        }
    }

    /// Whether `host` is the venue's domain or one of its subdomains.
    fn serves(self, host: &str) -> bool {
        let domain = self.domain();
        host.strip_suffix(domain).is_some_and(|prefix| prefix.is_empty() || prefix.ends_with('.'))
    }
}

impl NodeConfig {
    /// Reads and validates the config file at `path`.
    /// Error cases:
    /// - the file cannot be read, or is not valid TOML for this schema (unknown keys included)
    /// - `logging.level` is not a valid filter directive
    /// - `node.tick`, `node.checkpoint_every`, `node.segment_commands`, or `feeds.interval` is zero
    /// - `node.auth` is `none` with a TEE or custody provider other than `mock`
    /// - `feeds.venues` is missing for `provider = "venues"`, or fails validation
    /// - genesis parameters or a market spec fail validation
    /// - genesis house capital or a benchmark rate is one the engine would refuse
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let cfg: NodeConfig = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.validate().with_context(|| format!("validating {}", path.display()))?;
        Ok(cfg)
    }

    /// The client auth this config asks the log to record: unsigned, or signed under the domain of
    /// `custody.vault` on `custody.chain_id`.
    /// Error cases:
    /// - `custody.vault` is not an address
    pub fn client_auth(&self) -> anyhow::Result<ClientAuth> {
        Ok(match self.node.auth {
            AuthMode::None => ClientAuth::Unsigned,
            AuthMode::Eip712 => {
                let vault: AccountId = self.custody.vault.parse().context("custody.vault")?;
                ClientAuth::Eip712 { chain_id: self.custody.chain_id, vault }
            }
        })
    }

    /// Checks values that parse but would fail silently or at the wrong moment.
    /// Error cases:
    /// - `logging.level` is not a valid filter directive
    /// - `node.tick`, `node.checkpoint_every`, `node.segment_commands`, or `feeds.interval` is zero
    /// - `node.auth` is `none` with a TEE or custody provider other than `mock`
    /// - `feeds.venues` is missing for `provider = "venues"`, or fails validation
    /// - genesis parameters or a market spec fail validation
    /// - genesis house capital or a benchmark rate is one the engine would refuse
    fn validate(&self) -> anyhow::Result<()> {
        EnvFilter::try_new(&self.logging.level).context("logging.level")?;
        if self.node.tick.is_zero()
            || self.node.checkpoint_every == 0
            || self.node.segment_commands == 0
            || self.feeds.interval.is_zero()
        {
            bail!("node.tick, node.checkpoint_every, node.segment_commands, and feeds.interval must be non-zero");
        }
        match &self.feeds.venues {
            Some(venues) => {
                venues.validate().context("feeds.venues")?;
                self.check_stale_after(venues)?;
            }
            None if self.feeds.provider == FeedProvider::Venues => {
                bail!("feeds.provider = \"venues\" needs [feeds.venues]")
            }
            None => {}
        }
        // Unsigned requests let anyone act for any account, so they never run beside a real
        // enclave or real funds.
        if self.node.auth == AuthMode::None
            && (self.tee.provider != TeeProvider::Mock || self.custody.provider != CustodyProvider::Mock)
        {
            anyhow::bail!("node.auth = \"none\" needs tee.provider and custody.provider both \"mock\"");
        }
        self.genesis.params.validate().map_err(anyhow::Error::msg).context("genesis.params")?;
        // Genesis reruns on every boot until it completes, so a command the engine would refuse
        // fails here, before anything reaches the log.
        if !self.genesis.house_capital.is_valid_input() {
            anyhow::bail!("genesis.house_capital must be above zero and at most {}", Fixed::MAX_INPUT);
        }
        if let Some(b) =
            self.genesis.benchmarks.iter().find(|b| !(-MAX_BENCHMARK_BPS..=MAX_BENCHMARK_BPS).contains(&b.annual_bps))
        {
            anyhow::bail!("genesis benchmark {} is outside ±{MAX_BENCHMARK_BPS} bps", b.name);
        }
        for m in &self.genesis.markets {
            m.validate().map_err(anyhow::Error::msg).with_context(|| format!("market {}", m.symbol))?;
        }
        Ok(())
    }

    /// Checks `feeds.venues.stale_after` against the genesis market each venue symbol prices: a
    /// longer bound would count quotes the engine already drops as stale as the venue quoting.
    /// Error cases:
    /// - `stale_after` exceeds a priced market's `price_rules.max_age_ms`
    fn check_stale_after(&self, venues: &VenuesSection) -> anyhow::Result<()> {
        let priced: BTreeSet<MarketId> =
            venues.connections.iter().flat_map(|conn| conn.symbols.iter().map(|s| s.market)).collect();
        for market in self.genesis.markets.iter().filter(|market| priced.contains(&market.id)) {
            let max_age = u128::try_from(market.price_rules.max_age_ms).unwrap_or(0);
            ensure!(
                venues.stale_after.as_millis() <= max_age,
                "feeds.venues.stale_after exceeds market {} max_age_ms ({max_age})",
                market.symbol
            );
        }
        Ok(())
    }
}

impl VenuesSection {
    /// Checks the values that would otherwise fail silently: a venue quoting twice under one
    /// source name, a symbol that could mean two markets, or a feed the host could forge.
    /// Error cases:
    /// - `timeout`, `reconnect_min`, or `stale_after` is zero, or `reconnect_min` exceeds
    ///   `reconnect_max`
    /// - no connections, a venue listed twice, or a connection with no symbols
    /// - a URL that does not parse as `wss://` with a host on the venue's own domain
    /// - a market or symbol listed twice on one connection
    fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            !self.timeout.is_zero() && !self.reconnect_min.is_zero() && !self.stale_after.is_zero(),
            "timeout, reconnect_min, and stale_after must be non-zero"
        );
        ensure!(self.reconnect_min <= self.reconnect_max, "reconnect_min exceeds reconnect_max");
        ensure!(!self.connections.is_empty(), "no connections");
        let mut venues = BTreeSet::new();
        for conn in &self.connections {
            let name = conn.venue.name();
            ensure!(venues.insert(conn.venue), "{name} is listed twice");
            let uri: Uri = conn.url.parse().with_context(|| format!("{name} url"))?;
            ensure!(uri.scheme_str() == Some("wss"), "{name} url must be wss://");
            let domain = conn.venue.domain();
            ensure!(uri.host().is_some_and(|host| conn.venue.serves(host)), "{name} url must be on {domain}");
            ensure!(!conn.symbols.is_empty(), "{name} has no symbols");
            let mut markets = BTreeSet::new();
            let mut symbols = BTreeSet::new();
            for s in &conn.symbols {
                ensure!(markets.insert(s.market), "{name} lists market {} twice", s.market);
                ensure!(symbols.insert(s.symbol.as_str()), "{name} lists symbol {} twice", s.symbol);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::DEV_CONFIG as DEV;

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
    fn unsigned_requests_need_the_mock_tee_and_mock_custody() {
        let dev = std::fs::read_to_string(DEV).unwrap();
        let tee = dev.replace("provider = \"mock\" # \"configfs_tsm\"", "provider = \"configfs_tsm\" #");
        let custody = dev.replace("[custody]\nprovider = \"mock\"", "[custody]\nprovider = \"evm\"");
        for text in [&tee, &custody] {
            assert_ne!(text, &dev, "the replacement must change the config");
            let cfg: NodeConfig = toml::from_str(text).unwrap();
            let err = cfg.validate().unwrap_err();
            assert!(err.to_string().contains("node.auth = \"none\""), "{err:#}");
            let signed: NodeConfig = toml::from_str(&text.replace("auth = \"none\"", "auth = \"eip712\"")).unwrap();
            signed.validate().unwrap();
        }
    }

    #[test]
    fn signed_requests_take_their_domain_from_the_custody_chain_and_vault() {
        let dev = std::fs::read_to_string(DEV).unwrap();
        assert_eq!(toml::from_str::<NodeConfig>(&dev).unwrap().client_auth().unwrap(), ClientAuth::Unsigned);
        let vault = "0x5fbdb2315678afecb367f032d93f642f64180aa3";
        let signed = dev
            .replace("auth = \"none\"", "auth = \"eip712\"")
            .replace("chain_id = 31337", "chain_id = 42161")
            .replace("vault = \"0x0000000000000000000000000000000000000000\"", &format!("vault = \"{vault}\""));
        let cfg: NodeConfig = toml::from_str(&signed).unwrap();
        assert_eq!(cfg.client_auth().unwrap(), ClientAuth::Eip712 { chain_id: 42_161, vault: vault.parse().unwrap() });
    }

    #[test]
    fn genesis_values_the_engine_would_refuse_fail_the_boot() {
        let dev = std::fs::read_to_string(DEV).unwrap();
        let zero_capital = dev.replace("house_capital = \"10000000\"", "house_capital = \"0\"");
        let wild_rate = dev.replace("{ name = \"SOFR\", annual_bps = 400 }", "{ name = \"SOFR\", annual_bps = 10001 }");
        let min_rate = dev.replace(
            "{ name = \"SOFR\", annual_bps = 400 }",
            &format!("{{ name = \"SOFR\", annual_bps = {} }}", i64::MIN),
        );
        for text in [&zero_capital, &wild_rate, &min_rate] {
            assert_ne!(text, &dev, "the replacement must change the config");
            assert!(toml::from_str::<NodeConfig>(text).unwrap().validate().is_err());
        }
        let at_limit = dev.replace("{ name = \"SOFR\", annual_bps = 400 }", "{ name = \"SOFR\", annual_bps = -10000 }");
        toml::from_str::<NodeConfig>(&at_limit).unwrap().validate().unwrap();
    }

    #[test]
    fn a_zero_tick_is_refused() {
        let text = std::fs::read_to_string(DEV).unwrap().replace("tick = \"250ms\"", "tick = \"0s\"");
        let cfg: NodeConfig = toml::from_str(&text).unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn the_committed_venue_feeds_price_both_crypto_markets_from_three_venues() {
        let mut cfg = NodeConfig::load(Path::new(DEV)).unwrap();
        cfg.feeds.provider = FeedProvider::Venues;
        cfg.validate().unwrap();
        let connections = cfg.feeds.venues.unwrap().connections;
        let venues: Vec<Venue> = connections.iter().map(|c| c.venue).collect();
        assert_eq!(venues, [Venue::Coinbase, Venue::Kraken, Venue::Bitstamp]);
        for conn in connections {
            let markets: Vec<MarketId> = conn.symbols.iter().map(|s| s.market).collect();
            assert_eq!(markets, [1, 2], "{}", conn.venue.name());
        }
    }

    #[test]
    fn a_stale_after_beyond_a_priced_market_max_age_is_refused() {
        let mut cfg = NodeConfig::load(Path::new(DEV)).unwrap();
        cfg.feeds.venues.as_mut().unwrap().stale_after = Duration::from_millis(1_001);
        let err = format!("{:#}", cfg.validate().unwrap_err());
        assert!(err.contains("exceeds market BTC-USD max_age_ms (1000)"), "{err}");
    }

    #[test]
    fn venue_feeds_need_their_section() {
        let mut cfg = NodeConfig::load(Path::new(DEV)).unwrap();
        cfg.feeds.provider = FeedProvider::Venues;
        cfg.feeds.venues = None;
        let err = format!("{:#}", cfg.validate().unwrap_err());
        assert!(err.contains("needs [feeds.venues]"), "{err}");
    }

    #[test]
    fn venue_settings_that_would_fail_silently_are_refused() {
        type Edit = fn(&mut VenuesSection);
        let cases: [(&str, Edit); 13] = [
            ("non-zero", |v| v.timeout = Duration::ZERO),
            ("non-zero", |v| v.reconnect_min = Duration::ZERO),
            ("non-zero", |v| v.stale_after = Duration::ZERO),
            ("exceeds reconnect_max", |v| v.reconnect_max = v.reconnect_min / 2),
            ("no connections", |v| v.connections.clear()),
            ("coinbase is listed twice", |v| v.connections.push(v.connections[0].clone())),
            ("must be wss://", |v| v.connections[0].url = "ws://advanced-trade-ws.coinbase.com".into()),
            ("must be on coinbase.com", |v| v.connections[0].url = "wss://ws.kraken.com/v2".into()),
            ("must be on coinbase.com", |v| v.connections[0].url = "wss://coinbase.com.example.net".into()),
            ("must be on coinbase.com", |v| v.connections[0].url = "wss://evilcoinbase.com".into()),
            ("has no symbols", |v| v.connections[0].symbols.clear()),
            ("lists market 1 twice", |v| v.connections[0].symbols[1].market = 1),
            ("lists symbol BTC-USD twice", |v| v.connections[0].symbols[1].symbol = "BTC-USD".into()),
        ];
        for (expected, edit) in cases {
            let mut cfg = NodeConfig::load(Path::new(DEV)).unwrap();
            edit(cfg.feeds.venues.as_mut().unwrap());
            let err = format!("{:#}", cfg.validate().unwrap_err());
            assert!(err.contains(expected), "expected {expected:?}, got {err}");
        }
    }
}
