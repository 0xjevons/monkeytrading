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
use tokio::time::{Instant, MissedTickBehavior, interval_at, sleep, sleep_until, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{Connector, connect_async_tls_with_config};
use tracing::{debug, info, warn};

use crate::clock::now_ms;
use crate::config::{Venue, VenueConnection, VenuesSection};
use crate::feeds::{bitstamp, coinbase, kraken};
use crate::sequencer::Request;

/// After a refused-message alert, further refusals from the same venue only count until this has
/// passed, so a venue that changes its format raises one alert a minute, not one per message.
const REFUSAL_REALERT: Duration = Duration::from_secs(60);
/// How much of a refused message its log line keeps.
const EXCERPT_CHARS: usize = 200;
/// The furthest a venue timestamp may lead the host clock. The engine keeps each venue's newest
/// quote by time and skips one dated after the sequenced time, so a quote dated further ahead
/// would block that venue's real quotes until the host clock caught up with it.
const MAX_CLOCK_LEAD_MS: Millis = 1_000;

/// A venue's protocol on one connection: its subscribe messages, and a reader that keeps any
/// state the venue's messages build on, such as an order book.
pub(super) enum Adapter {
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

/// How often a venue task sequences quotes, and how it times out and reconnects.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// The most often one market's latest quote is sequenced.
    pub interval: Duration,
    pub timeout: Duration,
    pub reconnect_min: Duration,
    pub reconnect_max: Duration,
}

/// One venue's feed task and the alert state that outlives each connection.
struct VenueFeed {
    conn: VenueConnection,
    /// The connection's symbols, mapped to the markets they price.
    markets: BTreeMap<String, MarketId>,
    timing: Timing,
    tls: Arc<ClientConfig>,
    tx: mpsc::Sender<Request>,
    /// Whether the venue is in an alerted outage that no quote has ended yet.
    down: bool,
    /// Whether the current connection has delivered a quote.
    quoted: bool,
    /// When each market last got a quote on the current connection.
    last_quote: BTreeMap<MarketId, Instant>,
    /// Markets alerted as silent that have not quoted since.
    silent: BTreeSet<MarketId>,
    last_refusal_alert: Option<Instant>,
    refusals_since_alert: u64,
}

impl Timing {
    /// The timing from the node config.
    pub fn new(interval: Duration, venues: &VenuesSection) -> Self {
        Timing {
            interval,
            timeout: venues.timeout,
            reconnect_min: venues.reconnect_min,
            reconnect_max: venues.reconnect_max,
        }
    }
}

impl Adapter {
    /// A fresh adapter for a new connection to `venue`.
    pub(super) fn new(venue: Venue) -> Self {
        match venue {
            Venue::Bitstamp => Adapter::Bitstamp,
            Venue::Coinbase => Adapter::Coinbase(coinbase::Books::default()),
            Venue::Kraken => Adapter::Kraken,
        }
    }

    /// The text messages that subscribe a new connection to `symbols`.
    pub(super) fn subscriptions(&self, symbols: &[&str]) -> Vec<String> {
        match self {
            Adapter::Bitstamp => bitstamp::subscriptions(symbols),
            Adapter::Coinbase(_) => coinbase::subscriptions(symbols),
            Adapter::Kraken => kraken::subscriptions(symbols),
        }
    }

    /// Reads one venue message.
    /// Error cases:
    /// - the message is not one the venue's protocol defines, a field fails to parse, or the
    ///   venue reported an error
    /// - the message cannot be applied to the state kept for this connection
    pub(super) fn read(&mut self, text: &str) -> Result<Frame, Refusal> {
        match self {
            Adapter::Bitstamp => bitstamp::parse(text).map_err(Refusal::Message),
            Adapter::Coinbase(books) => books.read(text),
            Adapter::Kraken => kraken::parse(text).map_err(Refusal::Message),
        }
    }
}

impl VenueFeed {
    /// Connects and relays until the connection fails, then retries after a backoff that doubles
    /// while attempts deliver no quote and resets once one does.
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
            if self.quoted {
                backoff = self.timing.reconnect_min;
            }
            self.dropped(&error, backoff);
            sleep(backoff).await;
            backoff = next_backoff(backoff, self.timing.reconnect_max);
        }
    }

    /// One connection: connect, subscribe, then read messages and sequence the latest quote per
    /// market every `timing.interval`.
    /// Error cases:
    /// - connecting or subscribing fails or takes longer than `timing.timeout`
    /// - the venue sends nothing for `timing.timeout`, closes, or asks for a reconnect
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
        self.last_quote = self.markets.values().map(|market| (*market, subscribed)).collect();

        let mut pending: BTreeMap<MarketId, SourceQuote> = BTreeMap::new();
        let mut flush = interval_at(subscribed + self.timing.interval, self.timing.interval);
        flush.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut deadline = Instant::now() + self.timing.timeout;
        loop {
            tokio::select! {
                message = stream.next() => {
                    deadline = Instant::now() + self.timing.timeout;
                    match message.context("connection closed")?.context("read")? {
                        Message::Text(text) => self.receive(&mut adapter, text.as_str(), &mut pending)?,
                        Message::Binary(_) => self.refuse("binary message", ""),
                        Message::Close(frame) => bail!("closed by the venue: {frame:?}"),
                        Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
                    }
                }
                _ = flush.tick() => {
                    for (market, quote) in std::mem::take(&mut pending) {
                        let cmd = Command::Prices { market, quotes: vec![quote] };
                        self.tx.send(Request::Submit { cmd, reply: None }).await.map_err(|_| anyhow!("sequencer gone"))?;
                    }
                    self.check_silent();
                }
                () = sleep_until(deadline) => bail!("no message for {:?}", self.timing.timeout),
            }
        }
    }

    /// Reads one text message into `pending`, refusing what does not parse or check.
    /// Error cases:
    /// - the venue asked for a reconnect, or the adapter's state no longer matches the venue's
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
        let now = now_ms();
        for book in books {
            match to_quote(self.conn.venue, &self.markets, book, now) {
                Ok((market, quote)) => {
                    pending.insert(market, quote);
                    self.accepted(market);
                }
                Err(reason) => self.refuse(&reason, text),
            }
        }
        Ok(())
    }

    /// Records a quote for `market`, ending any outage or silence it was in.
    fn accepted(&mut self, market: MarketId) {
        let venue = self.conn.venue.name();
        self.quoted = true;
        self.last_quote.insert(market, Instant::now());
        if self.silent.remove(&market) {
            info!(venue, market, "venue symbol quoting again");
        }
        if self.down {
            self.down = false;
            info!(venue, "venue feed recovered");
        }
    }

    /// Alerts once for each symbol that has gone `timing.timeout` without a quote on this
    /// connection, such as one the venue accepted but never publishes.
    fn check_silent(&mut self) {
        let venue = self.conn.venue.name();
        let now = Instant::now();
        for (symbol, market) in &self.markets {
            let quiet = self.last_quote.get(market).is_some_and(|at| now.duration_since(*at) >= self.timing.timeout);
            if quiet && self.silent.insert(*market) {
                warn!(alert = "venue_symbol_silent", venue, symbol, market, "no quote for {:?}", self.timing.timeout);
            }
        }
    }

    /// Logs a refused message, alerting at most once per `REFUSAL_REALERT`.
    fn refuse(&mut self, reason: &str, text: &str) {
        let venue = self.conn.venue.name();
        let now = Instant::now();
        if self.last_refusal_alert.is_some_and(|at| now.duration_since(at) < REFUSAL_REALERT) {
            self.refusals_since_alert += 1;
            debug!(venue, reason, "venue message refused");
            return;
        }
        let excerpt: String = text.chars().take(EXCERPT_CHARS).collect();
        let suppressed = std::mem::take(&mut self.refusals_since_alert);
        warn!(alert = "venue_message_refused", venue, reason, suppressed, excerpt, "venue message refused");
        self.last_refusal_alert = Some(now);
    }

    /// Logs a lost connection. A connection that delivered quotes is a routine drop; one that
    /// delivered none starts an outage, alerted once until a quote ends it.
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
    let markets = conn.symbols.iter().map(|s| (s.symbol.clone(), s.market)).collect();
    let feed = VenueFeed {
        conn,
        markets,
        timing,
        tls,
        tx,
        down: false,
        quoted: false,
        last_quote: BTreeMap::new(),
        silent: BTreeSet::new(),
        last_refusal_alert: None,
        refusals_since_alert: 0,
    };
    feed.run().await
}

/// The venue's mid for one book and the market it prices, refusing a book no working venue
/// could show at host time `now`.
/// Error cases:
/// - the symbol is not one this connection subscribed to
/// - the bid or ask is outside the valid input range, or the book is crossed or locked
/// - the timestamp is before the epoch, or more than `MAX_CLOCK_LEAD_MS` after `now`
pub(super) fn to_quote(
    venue: Venue,
    markets: &BTreeMap<String, MarketId>,
    book: Book,
    now: Millis,
) -> Result<(MarketId, SourceQuote), String> {
    let Book { symbol, bid, ask, ts } = book;
    let market = *markets.get(&symbol).ok_or_else(|| format!("unsubscribed symbol {symbol}"))?;
    if !bid.is_valid_input() || !ask.is_valid_input() {
        return Err(format!("{symbol} bid {bid} or ask {ask} out of range"));
    }
    if bid >= ask {
        return Err(format!("{symbol} book crossed: bid {bid}, ask {ask}"));
    }
    let quote = SourceQuote { source: venue.name().to_owned(), mid: (bid + ask).div_int(2), ts };
    if !quote.is_well_formed() {
        return Err(format!("{symbol} timestamp {ts} before the epoch"));
    }
    if ts > now.saturating_add(MAX_CLOCK_LEAD_MS) {
        return Err(format!("{symbol} timestamp {ts} is {}ms ahead of the host clock", ts.saturating_sub(now)));
    }
    Ok((market, quote))
}

/// The delay after `current`: double it, capped at `max`.
pub(super) fn next_backoff(current: Duration, max: Duration) -> Duration {
    current.saturating_mul(2).min(max)
}
