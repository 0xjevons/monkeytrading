//! One venue's WebSocket feed. A task connects, subscribes, and sequences the venue's top-of-book
//! mids as `Command::Prices`, each stamped with the venue's own timestamp. It returns only when the
//! sequencer has gone away: a failed connection is retried inside the task with backoff, and while
//! it is down the venue's last quotes age out in the engine, which prices from the other venues.

use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use engine_core::Fixed;
use engine_core::command::Command;
use engine_core::price::SourceQuote;
use engine_core::types::{MarketId, Millis};
use futures_util::{SinkExt, StreamExt};
use rustls::crypto::ring;
use rustls::{ClientConfig, RootCertStore};
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior, interval_at, sleep, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{Connector, connect_async_tls_with_config};
use tracing::{debug, info, warn};

use crate::clock::now_ms;
use crate::config::{Venue, VenueConnection, VenuesSection};
use crate::feeds::{bitstamp, coinbase, kraken};
use crate::sequencer::Request;

/// After a refused-message or clock alert, the same alert from the same venue waits this long, so
/// a venue that changes its format raises one alert a minute, not one per message.
const REALERT_AFTER: Duration = Duration::from_secs(60);
/// How much of a refused message its log line keeps.
const EXCERPT_CHARS: usize = 200;
/// A quote dated this far ahead of the host clock raises `venue_clock_ahead`: the venue's clock or
/// the host's is wrong. The quote is still sequenced, so the engine, which skips a quote dated
/// after the sequenced time, prices nothing from it rather than keep using older quotes.
///
/// TODO: the sequencer stamps with the untrusted host clock; once a trusted time bound exists, a
/// host clock this far behind the venues should fail closed there.
const MAX_CLOCK_LEAD_MS: Millis = 1_000;

/// A venue's protocol on one connection: its subscribe messages, and a reader that keeps any
/// state the venue's messages build on, such as an order book.
enum Adapter {
    Bitstamp,
    Coinbase(coinbase::Books),
    Kraken,
}

/// Why an adapter refused a message.
#[derive(Debug, PartialEq)]
pub(super) enum Refusal {
    /// The message is dropped and the connection carries on.
    Message(String),
    /// The connection's state no longer matches the venue's, so it reconnects.
    Connection(String),
}

/// What one venue message carries, as its adapter reads it.
#[derive(Debug, PartialEq)]
pub(super) enum Frame {
    /// The best bid and ask for one or more of the venue's symbols.
    Books(Vec<Book>),
    /// An acknowledgement, heartbeat, or status message: nothing to price.
    Control,
    /// The venue asked for a reconnect, so this connection ends.
    Reconnect,
}

/// One venue's best bid and ask for a symbol, at the venue's own time.
#[derive(Debug, PartialEq)]
pub(super) struct Book {
    pub symbol: String,
    pub bid: Fixed,
    pub ask: Fixed,
    pub ts: Millis,
}

/// How often a venue task sequences quotes, how it times out and reconnects, and how old a quote
/// may be and still count as the venue quoting.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// The most often one market's latest quote is sequenced.
    pub interval: Duration,
    pub timeout: Duration,
    pub reconnect_min: Duration,
    pub reconnect_max: Duration,
    pub stale_after: Duration,
}

/// One venue's feed task and the alert state that outlives each connection.
struct VenueFeed {
    conn: VenueConnection,
    /// The connection's symbols, mapped to the markets they price.
    markets: BTreeMap<String, MarketId>,
    timing: Timing,
    tls: Arc<ClientConfig>,
    tx: mpsc::Sender<Request>,
    /// Whether the venue is in an alerted outage that no fresh quote has ended yet.
    down: bool,
    /// Whether the current connection has sequenced a fresh quote.
    quoted: bool,
    /// When each market last had a fresh quote sequenced, counted from the task's start or the
    /// venue's last recovery, and kept across routine reconnects.
    last_quote: BTreeMap<MarketId, Instant>,
    /// Markets alerted as silent that have not quoted since.
    silent: BTreeSet<MarketId>,
    last_refusal_alert: Option<Instant>,
    refusals_since_alert: u64,
    last_clock_alert: Option<Instant>,
}

impl Timing {
    /// The timing from the node config.
    pub fn new(interval: Duration, venues: &VenuesSection) -> Self {
        Timing {
            interval,
            timeout: venues.timeout,
            reconnect_min: venues.reconnect_min,
            reconnect_max: venues.reconnect_max,
            stale_after: venues.stale_after,
        }
    }
}

impl Adapter {
    /// A fresh adapter for a new connection to `venue`.
    fn new(venue: Venue) -> Self {
        match venue {
            Venue::Bitstamp => Adapter::Bitstamp,
            Venue::Coinbase => Adapter::Coinbase(coinbase::Books::default()),
            Venue::Kraken => Adapter::Kraken,
        }
    }

    /// The text messages that subscribe a new connection to `symbols`.
    fn subscriptions(&self, symbols: &[&str]) -> Vec<String> {
        match self {
            Adapter::Bitstamp => bitstamp::subscriptions(symbols),
            Adapter::Coinbase(_) => coinbase::subscriptions(symbols),
            Adapter::Kraken => kraken::subscriptions(symbols),
        }
    }

    /// Whether the adapter keeps state built from earlier messages, so a refused book means that
    /// state can no longer be trusted.
    fn keeps_state(&self) -> bool {
        matches!(self, Adapter::Coinbase(_))
    }

    /// Reads one venue message.
    /// Error cases:
    /// - the message is not one the venue's protocol defines, a field fails to parse, or the
    ///   venue reported an error
    /// - the message cannot be applied to the state kept for this connection
    fn read(&mut self, text: &str) -> Result<Frame, Refusal> {
        match self {
            Adapter::Bitstamp => bitstamp::parse(text).map_err(Refusal::Message),
            Adapter::Coinbase(books) => books.read(text),
            Adapter::Kraken => kraken::parse(text).map_err(Refusal::Message),
        }
    }
}

impl VenueFeed {
    /// A feed for `conn`, with no outage, refusal, or silence on record, and every market's
    /// silence counted from now.
    fn new(conn: VenueConnection, timing: Timing, tls: Arc<ClientConfig>, tx: mpsc::Sender<Request>) -> Self {
        let markets: BTreeMap<String, MarketId> = conn.symbols.iter().map(|s| (s.symbol.clone(), s.market)).collect();
        let started = Instant::now();
        let last_quote = markets.values().map(|market| (*market, started)).collect();
        VenueFeed {
            conn,
            markets,
            timing,
            tls,
            tx,
            down: false,
            quoted: false,
            last_quote,
            silent: BTreeSet::new(),
            last_refusal_alert: None,
            refusals_since_alert: 0,
            last_clock_alert: None,
        }
    }

    /// Connects and relays until the connection fails, then retries after a backoff that doubles
    /// while attempts sequence no fresh quote and resets once one does.
    /// Error cases:
    /// - the sequencer has gone away
    async fn run(mut self) -> anyhow::Result<()> {
        let mut backoff = self.timing.reconnect_min;
        loop {
            self.quoted = false;
            let Err(error) = self.relay().await;
            if self.tx.is_closed() {
                return Err(error.context("sequencer gone"));
            }
            let (wait, next) = next_attempt(backoff, self.quoted, &self.timing);
            self.dropped(&error, wait);
            sleep(wait).await;
            backoff = next;
        }
    }

    /// One connection: connect, subscribe, then read messages and sequence the newest quote per
    /// market every `timing.interval`. A quote no older than `timing.stale_after` when sequenced
    /// counts as the venue quoting that market.
    /// Error cases:
    /// - connecting or subscribing fails or takes longer than `timing.timeout`
    /// - the connection sequences no fresh quote within `timing.timeout`, or the venue closes it
    ///   or asks for a reconnect
    /// - a symbol goes silent, so the connection is replaced to resubscribe it
    /// - the connection fails, or the sequencer has gone away
    async fn relay(&mut self) -> anyhow::Result<Infallible> {
        let connector = Some(Connector::Rustls(self.tls.clone()));
        let connect = connect_async_tls_with_config(self.conn.url.as_str(), None, false, connector);
        let (ws, _) = timeout(self.timing.timeout, connect).await.context("connect timed out")?.context("connect")?;
        let (mut sink, mut stream) = ws.split();
        let mut adapter = Adapter::new(self.conn.venue);
        let symbols: Vec<&str> = self.markets.keys().map(String::as_str).collect();
        let subscribe = async {
            for text in adapter.subscriptions(&symbols) {
                sink.send(Message::text(text)).await?;
            }
            anyhow::Ok(())
        };
        timeout(self.timing.timeout, subscribe).await.context("subscribe timed out")?.context("subscribe")?;
        info!(venue = self.conn.venue.name(), "venue subscribed");
        let subscribed = Instant::now();

        let mut pending: BTreeMap<MarketId, SourceQuote> = BTreeMap::new();
        let mut flush = interval_at(subscribed + self.timing.interval, self.timing.interval);
        flush.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let stale_after = Millis::try_from(self.timing.stale_after.as_millis()).unwrap_or(Millis::MAX);
        loop {
            tokio::select! {
                message = stream.next() => {
                    match message.context("connection closed")?.context("read")? {
                        Message::Text(text) => self.receive(&mut adapter, text.as_str(), &mut pending)?,
                        Message::Binary(_) => self.refuse("binary message", ""),
                        Message::Close(frame) => bail!("closed by the venue: {frame:?}"),
                        Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
                    }
                }
                _ = flush.tick() => {
                    let now = now_ms();
                    for (market, quote) in std::mem::take(&mut pending) {
                        // As the engine judges it: not after the host clock, and no older than the bound.
                        let fresh = quote.ts <= now && now - quote.ts <= stale_after;
                        if quote.ts > now.saturating_add(MAX_CLOCK_LEAD_MS) {
                            self.clock_ahead(market, quote.ts - now);
                        }
                        let cmd = Command::Prices { market, quotes: vec![quote] };
                        self.tx.send(Request::Submit { cmd, reply: None }).await.map_err(|_| anyhow!("sequencer gone"))?;
                        if fresh {
                            self.delivered(market);
                        }
                    }
                    if self.check_silent() {
                        bail!("resubscribing after a symbol went silent");
                    }
                    // Covers a connection that stays open but carries nothing usable: silent,
                    // heartbeats only, or every symbol refused, even while the venue is down.
                    if !self.quoted && subscribed.elapsed() >= self.timing.timeout {
                        bail!("no fresh quote for {:?}", self.timing.timeout);
                    }
                }
            }
        }
    }

    /// Reads one text message into `pending`, refusing what does not parse or check.
    /// Error cases:
    /// - the venue asked for a reconnect, or the adapter's state no longer matches the venue's,
    ///   including a refused book from an adapter that keeps state
    fn receive(
        &mut self,
        adapter: &mut Adapter,
        text: &str,
        pending: &mut BTreeMap<MarketId, SourceQuote>,
    ) -> anyhow::Result<()> {
        let books = match adapter.read(text) {
            Ok(Frame::Books(books)) => books,
            Ok(Frame::Control) => return Ok(()),
            Ok(Frame::Reconnect) => bail!("the venue asked for a reconnect"),
            Err(Refusal::Message(reason)) => {
                self.refuse(&reason, text);
                return Ok(());
            }
            Err(Refusal::Connection(reason)) => {
                self.refuse(&reason, text);
                bail!("resyncing: {reason}");
            }
        };
        for book in books {
            match to_quote(self.conn.venue, &self.markets, book) {
                Ok((market, quote)) => {
                    // Keep the newest by venue time, as the engine does, whatever order they arrive in.
                    if pending.get(&market).is_none_or(|held| quote.ts >= held.ts) {
                        pending.insert(market, quote);
                    }
                }
                Err(reason) if adapter.keeps_state() => {
                    self.refuse(&reason, text);
                    bail!("resyncing: {reason}");
                }
                Err(reason) => self.refuse(&reason, text),
            }
        }
        Ok(())
    }

    /// Records a fresh quote sequenced for `market`, ending any outage or silence it was in. A
    /// recovery restarts every market's silence count, since the outage alert covered the gap.
    fn delivered(&mut self, market: MarketId) {
        let venue = self.conn.venue.name();
        let now = Instant::now();
        self.quoted = true;
        if self.down {
            self.down = false;
            self.last_quote.values_mut().for_each(|at| *at = now);
            info!(venue, "venue feed recovered");
        }
        self.last_quote.insert(market, now);
        if self.silent.remove(&market) {
            info!(venue, market, "venue symbol quoting again");
        }
    }

    /// Alerts once for each symbol that has gone `timing.timeout` without a fresh quote sequenced,
    /// across reconnects: one the venue accepted but never publishes, or whose quotes arrive stale
    /// or ahead of the host clock. An outage already alerted stands for every symbol.
    /// Returns whether a symbol went silent on this check, so the connection is replaced once to
    /// subscribe it afresh; a symbol already silent does not trigger another.
    fn check_silent(&mut self) -> bool {
        if self.down {
            return false;
        }
        let venue = self.conn.venue.name();
        let now = Instant::now();
        let mut newly_silent = false;
        for (symbol, market) in &self.markets {
            let quiet = self.last_quote.get(market).is_some_and(|at| now.duration_since(*at) >= self.timing.timeout);
            if quiet && self.silent.insert(*market) {
                warn!(
                    alert = "venue_symbol_silent",
                    venue, symbol, market, "no fresh quote for {:?}", self.timing.timeout
                );
                newly_silent = true;
            }
        }
        newly_silent
    }

    /// Alerts, at most once per `REALERT_AFTER`, that a quote for `market` is dated `lead_ms` ahead
    /// of the host clock.
    fn clock_ahead(&mut self, market: MarketId, lead_ms: Millis) {
        let now = Instant::now();
        if self.last_clock_alert.is_some_and(|at| now.duration_since(at) < REALERT_AFTER) {
            return;
        }
        let venue = self.conn.venue.name();
        warn!(alert = "venue_clock_ahead", venue, market, lead_ms, "venue time ahead of the host clock");
        self.last_clock_alert = Some(now);
    }

    /// Logs a refused message, alerting at most once per `REALERT_AFTER`.
    fn refuse(&mut self, reason: &str, text: &str) {
        let venue = self.conn.venue.name();
        let now = Instant::now();
        if self.last_refusal_alert.is_some_and(|at| now.duration_since(at) < REALERT_AFTER) {
            self.refusals_since_alert += 1;
            debug!(venue, reason, "venue message refused");
            return;
        }
        let excerpt: String = text.chars().take(EXCERPT_CHARS).collect();
        let suppressed = std::mem::take(&mut self.refusals_since_alert);
        warn!(alert = "venue_message_refused", venue, reason, suppressed, excerpt, "venue message refused");
        self.last_refusal_alert = Some(now);
    }

    /// Logs a lost connection. A connection that sequenced fresh quotes is a routine drop; one that
    /// sequenced none starts an outage, alerted once until a fresh quote ends it.
    fn dropped(&mut self, error: &anyhow::Error, retry_in: Duration) {
        let venue = self.conn.venue.name();
        if self.quoted {
            info!(venue, ?retry_in, "venue feed dropped: {error:#}");
        } else if !self.down {
            self.down = true;
            warn!(alert = "venue_feed_down", venue, ?retry_in, "venue feed down: {error:#}");
        } else {
            debug!(venue, ?retry_in, "venue still down: {error:#}");
        }
    }
}

/// The TLS client config every venue connection shares: ring as the crypto provider, and
/// Mozilla's roots compiled into the binary, so the host's certificate store is never trusted.
/// Error cases:
/// - the provider supports none of the safe default protocol versions
pub fn tls_config() -> anyhow::Result<Arc<ClientConfig>> {
    let roots = RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let config = ClientConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()
        .context("TLS protocol versions")?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// Streams one venue's quotes into the sequencer until the sequencer goes away.
/// Error cases:
/// - the sequencer has gone away
pub async fn run(
    conn: VenueConnection,
    timing: Timing,
    tls: Arc<ClientConfig>,
    tx: mpsc::Sender<Request>,
) -> anyhow::Result<()> {
    VenueFeed::new(conn, timing, tls, tx).run().await
}

/// The venue's mid for one book and the market it prices, refusing a book no working venue
/// could show.
/// Error cases:
/// - the symbol is not one this connection subscribed to
/// - the bid or ask is outside the valid input range, or the book is crossed or locked
/// - the timestamp is before the epoch
pub(super) fn to_quote(
    venue: Venue,
    markets: &BTreeMap<String, MarketId>,
    book: Book,
) -> Result<(MarketId, SourceQuote), String> {
    let Book { symbol, bid, ask, ts } = book;
    let market = *markets.get(&symbol).ok_or_else(|| format!("unsubscribed symbol {symbol}"))?;
    if !bid.is_valid_input() || !ask.is_valid_input() {
        return Err(format!("{symbol} bid {bid} or ask {ask} out of range"));
    }
    if bid >= ask {
        return Err(format!("{symbol} book crossed: bid {bid}, ask {ask}"));
    }
    check_time(&symbol, ts)?;
    Ok((market, SourceQuote { source: venue.name().to_owned(), mid: (bid + ask).div_int(2), ts }))
}

/// Refuses a top-of-book size no working venue could show.
/// Error cases:
/// - `size` is not positive, or is above `Fixed::MAX_INPUT`
pub(super) fn check_size(symbol: &str, side: &str, size: Fixed) -> Result<(), String> {
    if !size.is_valid_input() {
        return Err(format!("{symbol} {side} size {size} out of range"));
    }
    Ok(())
}

/// Refuses a venue time no working venue could send. A time ahead of the host clock is not
/// refused: refusing it would leave the engine on older quotes when the host clock is behind.
/// Error cases:
/// - `ts` is before the epoch
pub(super) fn check_time(symbol: &str, ts: Millis) -> Result<(), String> {
    if ts < 0 {
        return Err(format!("{symbol} timestamp {ts} before the epoch"));
    }
    Ok(())
}

/// The wait before the next connection attempt, and the backoff after that. A connection that
/// sequenced a fresh quote retries after `reconnect_min`; otherwise the wait is `backoff`, and the
/// next one doubles it up to `reconnect_max`.
pub(super) fn next_attempt(backoff: Duration, quoted: bool, timing: &Timing) -> (Duration, Duration) {
    let wait = if quoted { timing.reconnect_min } else { backoff };
    (wait, next_backoff(wait, timing.reconnect_max))
}

/// The delay after `current`: double it, capped at `max`.
pub(super) fn next_backoff(current: Duration, max: Duration) -> Duration {
    current.saturating_mul(2).min(max)
}

#[cfg(test)]
mod tests;
