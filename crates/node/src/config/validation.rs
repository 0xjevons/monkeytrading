//! The checks a node config must pass once it parses: values that would fail silently or at the
//! wrong moment.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use engine_core::Fixed;
use engine_core::params::MAX_BENCHMARK_BPS;
use tokio_tungstenite::tungstenite::http::Uri;
use tracing_subscriber::EnvFilter;

use crate::config::{AuthMode, CustodyProvider, FeedProvider, NodeConfig, TeeProvider, VenuesSection};
use crate::trusted_time::NO_SAMPLE_GRACE;

impl NodeConfig {
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
    /// - the custody section does not match its provider (see `validate_custody`)
    /// - `operator.public_key` or `custody.vault` does not parse
    /// - genesis parameters fail validation or let spreads refuse a close, or a market spec fails
    ///   validation
    /// - genesis house capital is negative or above `Fixed::MAX_INPUT`, a benchmark rate is one the
    ///   engine would refuse, or genesis lists a market id twice
    /// - a genesis market fails `MarketSpec::check_fx_market` against the markets listed before it
    pub(super) fn validate(&self) -> anyhow::Result<()> {
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
        self.validate_custody()?;
        self.operator()?;
        self.genesis.params.validate().map_err(anyhow::Error::msg).context("genesis.params")?;
        // Genesis runs under the latest rules, which refuse these.
        self.genesis.params.check_closing_spread().map_err(anyhow::Error::msg).context("genesis.params")?;
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
        let mut listed = BTreeMap::new();
        for m in &self.genesis.markets {
            m.validate().map_err(anyhow::Error::msg).with_context(|| format!("market {}", m.symbol))?;
            // A market quoted outside USD converts at its pair's market, listed before it.
            m.check_fx_market(|id| listed.get(&id).copied())
                .map_err(anyhow::Error::msg)
                .with_context(|| format!("market {}", m.symbol))?;
            listed.insert(m.id, m.symbol.as_str());
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

/// Whether `address` is a non-empty host, a colon, and a port number.
fn is_host_port(address: &str) -> bool {
    address.rsplit_once(':').is_some_and(|(host, port)| !host.is_empty() && port.parse::<u16>().is_ok())
}
