//! Node configuration: one TOML file, loaded and validated once at startup. Two parts are protocol
//! and go into the log: the client auth (`node.auth`, with `custody.chain_id` and `custody.vault`
//! under `eip712`), recorded when the log first activates protocol versions and fixed for the life
//! of the log; and the `[genesis]` section, sequenced after protocol activation. Everything else
//! configures this node only.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::VerifyingKey;
use engine_core::Fixed;
use engine_core::auth::ClientAuth;
use engine_core::market::MarketSpec;
use engine_core::params::{MAX_BENCHMARK_BPS, ProtocolParams};
use engine_core::types::{AccountId, MarketId, Millis};
use serde::de::Error;
use serde::{Deserialize, Deserializer};
use tokio_tungstenite::tungstenite::http::Uri;
use tracing_subscriber::EnvFilter;

use crate::trusted_time::NO_SAMPLE_GRACE;

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
    pub api: ApiSection,
    pub logging: LoggingSection,
    pub tee: TeeSection,
    pub operator: OperatorSection,
    pub custody: CustodySection,
    pub feeds: FeedsSection,
    pub time: TimeSection,
    pub history: HistorySection,
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

/// Limits on the public API. A client is its socket peer IP, with an IPv6 client counted by its
/// /64; forwarding headers are ignored because the host could forge them.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiSection {
    /// Open connections, in total and from one client; one over either is closed unread. Keep the
    /// total below the process's open-file limit, or accepts fail before the cap is reached.
    pub max_connections: NonZeroUsize,
    pub max_connections_per_ip: NonZeroUsize,
    /// A connection that has not finished its TLS handshake by then is closed.
    #[serde(with = "humantime_serde")]
    pub handshake_timeout: Duration,
    /// A client gets this long to send a request's headers, and again its body; a connection idle
    /// this long between requests is closed.
    #[serde(with = "humantime_serde")]
    pub read_timeout: Duration,
    /// Largest request body, which gets 413 when larger, and largest message a stream subscriber
    /// may send, which ends its stream when larger.
    pub max_body_bytes: NonZeroUsize,
    /// Requests from one client, and requests that act for one account; excess requests get 429.
    pub per_ip: RateLimit,
    pub per_account: RateLimit,
    /// New connections from one client; one over it is closed before its TLS handshake. Set it
    /// above `per_ip`, so a client that opens a connection per request gets 429 first.
    pub per_ip_connections: RateLimit,
    /// Open WebSocket stream subscriptions; one more gets 503.
    pub max_subscribers: NonZeroUsize,
    /// A client that takes longer than this to take a response, or a stream subscriber one
    /// message, is disconnected.
    #[serde(with = "humantime_serde")]
    pub send_timeout: Duration,
}

/// A token bucket: `burst` requests at once, refilled at `per_second`.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimit {
    pub per_second: NonZeroU32,
    pub burst: NonZeroU32,
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
    /// A quote older than this, or than its market's `max_age_ms` in the engine, once sequenced
    /// does not count as its venue quoting: it ends no outage or silence.
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

/// Trusted time: the Roughtime servers the host clock is checked against.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimeSection {
    /// The most a sequenced timestamp may differ from Roughtime-verified time. A reply slower than
    /// this cannot place the clock within it, so it is also how long each query waits.
    #[serde(with = "humantime_serde")]
    pub max_drift: Duration,
    /// How often to query the servers.
    #[serde(with = "humantime_serde")]
    pub interval: Duration,
    /// Servers run by independent operators; a sample needs replies from a strict majority.
    ///
    /// TODO: this file sits on host storage, so these keys are trusted only once the attestation
    /// binds the config's hash (#26).
    pub servers: Vec<RoughtimeServer>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoughtimeServer {
    pub name: String,
    /// `host:port`, queried over UDP.
    pub address: String,
    /// The server's long-term Ed25519 public key, in base64 as its operator publishes it.
    #[serde(deserialize_with = "public_key")]
    pub public_key: VerifyingKey,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistorySection {
    /// Where the node keeps the account history index, which it derives from the log alone:
    /// removing the directory while the node is stopped rebuilds the index from the log.
    pub dir: PathBuf,
    /// How long an event stays in the index, counted back from the sequenced time of the newest
    /// command it holds. The log keeps every event regardless. After a change, the index takes the
    /// log again from its start.
    #[serde(with = "humantime_serde")]
    pub retention: Duration,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Genesis {
    /// House capital credited at genesis. Zero credits none.
    /// TODO: nothing checks this against the house's vault deposit; bound it once custody reads it.
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
        host.strip_suffix(self.domain()).is_some_and(|prefix| prefix.is_empty() || prefix.ends_with('.'))
    }
}

impl NodeConfig {
    /// Reads and validates the config file at `path`.
    /// Error cases:
    /// - the file cannot be read, or is not valid TOML for this schema (unknown keys included)
    /// - `logging.level` is not a valid filter directive
    /// - `node.tick`, `node.checkpoint_every`, `node.segment_commands`, `feeds.interval`,
    ///   `time.max_drift`, `time.interval`, or an `api` timeout is zero, or `time.interval` plus
    ///   twice `time.max_drift` is not under 60s
    /// - `node.auth` is `none` with a TEE or custody provider other than `mock`
    /// - `feeds.venues` is missing for `provider = "venues"`, fails validation, or has a `timeout`
    ///   no longer than `feeds.interval`
    /// - `time.servers` has fewer than two servers, lists a public key twice, or has an address
    ///   that is not `host:port`
    /// - `history.retention` is zero, or too long to count in milliseconds
    /// - genesis parameters or a market spec fail validation
    /// - genesis house capital is negative or above `Fixed::MAX_INPUT`, a benchmark rate is one the
    ///   engine would refuse, or genesis lists a market id twice
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
    /// - `node.tick`, `node.checkpoint_every`, `node.segment_commands`, `feeds.interval`,
    ///   `time.max_drift`, `time.interval`, or an `api` timeout is zero, or `time.interval` plus
    ///   twice `time.max_drift` is not under 60s
    /// - `node.auth` is `none` with a TEE or custody provider other than `mock`
    /// - `feeds.venues` is missing for `provider = "venues"`, fails validation, or has a `timeout`
    ///   no longer than `feeds.interval`
    /// - `time.servers` has fewer than two servers, lists a public key twice, or has an address
    ///   that is not `host:port`
    /// - `history.retention` is zero, or too long to count in milliseconds
    /// - genesis parameters or a market spec fail validation
    /// - genesis house capital is negative or above `Fixed::MAX_INPUT`, a benchmark rate is one the
    ///   engine would refuse, or genesis lists a market id twice
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
                // Silence is checked once an interval, so a longer interval would outlast the timeout.
                ensure!(
                    self.feeds.interval < venues.timeout,
                    "feeds.interval must be shorter than feeds.venues.timeout"
                );
            }
            None if self.feeds.provider == FeedProvider::Venues => {
                bail!("feeds.provider = \"venues\" needs [feeds.venues]")
            }
            None => {}
        }
        if [self.api.handshake_timeout, self.api.read_timeout, self.api.send_timeout].iter().any(Duration::is_zero) {
            anyhow::bail!("api.handshake_timeout, api.read_timeout, and api.send_timeout must be non-zero");
        }
        // Unsigned requests let anyone act for any account, so they never run beside a real
        // enclave or real funds.
        if self.node.auth == AuthMode::None
            && (self.tee.provider != TeeProvider::Mock || self.custody.provider != CustodyProvider::Mock)
        {
            anyhow::bail!("node.auth = \"none\" needs tee.provider and custody.provider both \"mock\"");
        }
        if self.time.max_drift.is_zero() || self.time.interval.is_zero() {
            anyhow::bail!("time.max_drift and time.interval must be non-zero");
        }
        // A round waits up to `max_drift` for a silent server, so a sample may be that old when it
        // arrives, and the next may come `interval` plus `max_drift` later. All of it must fit in
        // the grace, or a node with one silent server halts while the others agree.
        let round = self.time.interval.saturating_add(self.time.max_drift.saturating_mul(2));
        if round >= NO_SAMPLE_GRACE {
            anyhow::bail!("time.interval plus twice time.max_drift must be under {NO_SAMPLE_GRACE:?}");
        }
        // One server, or one key listed twice, would let a single operator set the time.
        let servers = &self.time.servers;
        if servers.len() < 2 {
            anyhow::bail!("time.servers needs at least two servers, so that more than one must agree");
        }
        if servers.iter().enumerate().any(|(i, a)| servers[..i].iter().any(|b| a.public_key == b.public_key)) {
            anyhow::bail!("time.servers lists the same public key twice");
        }
        if let Some(s) = servers.iter().find(|s| !is_host_port(&s.address)) {
            anyhow::bail!("time.servers address {:?} is not host:port", s.address);
        }
        self.history.retention_ms().context("history.retention")?;
        self.genesis.params.validate().map_err(anyhow::Error::msg).context("genesis.params")?;
        // Genesis reruns on every boot until it completes, so a command the engine would refuse
        // fails here, before anything reaches the log.
        // Zero records the genesis credit and moves nothing; any other amount must be one the engine
        // accepts.
        if self.genesis.house_capital != Fixed::ZERO && !self.genesis.house_capital.is_valid_input() {
            anyhow::bail!("genesis.house_capital must be zero, or above zero and at most {}", Fixed::MAX_INPUT);
        }
        // A repeated id would silently replace the earlier listing.
        let mut ids = BTreeSet::new();
        if let Some(m) = self.genesis.markets.iter().find(|m| !ids.insert(m.id)) {
            anyhow::bail!("genesis lists market {} twice", m.id);
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

impl HistorySection {
    /// The retention in milliseconds, the unit of sequenced time.
    /// Error cases:
    /// - the retention is zero, which would empty the index at every write
    /// - it is too long to count in milliseconds
    pub fn retention_ms(&self) -> anyhow::Result<Millis> {
        ensure!(!self.retention.is_zero(), "must be non-zero");
        Millis::try_from(self.retention.as_millis()).context("too long")
    }
}

/// Whether `address` is a non-empty host, a colon, and a port number.
fn is_host_port(address: &str) -> bool {
    address.rsplit_once(':').is_some_and(|(host, port)| !host.is_empty() && port.parse::<u16>().is_ok())
}

/// Decodes a base64 Ed25519 public key.
/// Error cases:
/// - the text is not base64, does not decode to 32 bytes, or is not a valid Ed25519 key
fn public_key<'de, D: Deserializer<'de>>(deserializer: D) -> Result<VerifyingKey, D::Error> {
    let bytes = STANDARD.decode(String::deserialize(deserializer)?).map_err(D::Error::custom)?;
    let bytes: [u8; 32] = bytes.try_into().map_err(|_| D::Error::custom("an Ed25519 public key is 32 bytes"))?;
    VerifyingKey::from_bytes(&bytes).map_err(D::Error::custom)
}

#[cfg(test)]
mod tests;
