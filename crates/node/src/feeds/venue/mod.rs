//! One venue's WebSocket feed. A task connects, subscribes, and sequences the venue's top-of-book
//! mids as `Command::Prices`, each stamped with the venue's own timestamp. It returns only when the
//! sequencer has gone away: a failed connection is retried inside the task with backoff, and while
//! it is down the venue's last quotes age out in the engine, which prices from the other venues.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::convert::Infallible;
use std::mem::take;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use engine_core::Fixed;
use engine_core::command::Command;
use engine_core::price::{MAX_QUOTE_LEAD_MS, SourceQuote};
use engine_core::types::{MarketId, Millis};
use futures_util::{SinkExt, StreamExt};
use rustls::ClientConfig;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, MissedTickBehavior, interval_at, sleep, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{Connector, connect_async_tls_with_config};
use tracing::{debug, info, warn};

use crate::clock::now_ms;
use crate::config::{Venue, VenueConnection, VenuesSection};
use crate::feeds::{bitstamp, coinbase, kraken};
use crate::sequencer::{Query, Request};
mod setup;
pub use setup::tls_config;
mod validation;
pub(super) use validation::{check_size, check_time, to_quote};

/// After a refused-message or clock alert, the same alert from the same venue waits this long, so
/// a venue that changes its format raises one alert a minute, not one per message.
const REALERT_AFTER: Duration = Duration::from_secs(60);
/// How much of a refused message its log line keeps.
const EXCERPT_CHARS: usize = 200;
/// The most quotes kept per market between two sequencing ticks; the oldest go first.
const MAX_PENDING: usize = 256;
/// A venue's protocol on one connection: its subscribe messages, and a reader that keeps any
/// state the venue's messages build on, such as an order book.
enum Adapter {
    Bitstamp,
    Coinbase(coinbase::Books),
    Kraken(kraken::Books),
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
    /// The time of the quote the engine holds from this venue per market, as far as this task
    /// knows, seeded from the engine when the task starts. A quote the engine will not take in
    /// its place is never counted as the venue quoting.
    held: BTreeMap<MarketId, Millis>,
    refusals: Throttle,
    clock_alerts: Throttle,
}

/// Holds an alert to one per `REALERT_AFTER`, counting the ones it held back.
#[derive(Default)]
struct Throttle {
    last: Option<Instant>,
    held: u64,
}

impl Throttle {
    /// Whether to alert at `now`, and if so, how many alerts it held back since the last one.
    fn ready(&mut self, now: Instant) -> Option<u64> {
        if self.last.is_some_and(|at| now.duration_since(at) < REALERT_AFTER) {
            self.held += 1;
            return None;
        }
        self.last = Some(now);
        Some(take(&mut self.held))
    }
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
            Venue::Kraken => Adapter::Kraken(kraken::Books::default()),
        }
    }

    /// The text messages that subscribe a new connection to `symbols`.
    fn subscriptions(&self, symbols: &[&str]) -> Vec<String> {
        match self {
            Adapter::Bitstamp => bitstamp::subscriptions(symbols),
            Adapter::Coinbase(_) => coinbase::subscriptions(symbols),
            Adapter::Kraken(_) => kraken::subscriptions(symbols),
        }
    }

    /// Whether the adapter keeps state built from earlier messages, so a refused book means that
    /// state can no longer be trusted.
    fn keeps_state(&self) -> bool {
        matches!(self, Adapter::Coinbase(_) | Adapter::Kraken(_))
    }

    /// Reads one venue message at host time `now`.
    /// Error cases:
    /// - the message is not one the venue's protocol defines, a field fails to parse, or the
    ///   venue reported an error
    /// - the message cannot be applied to the state kept for this connection
    fn read(&mut self, text: &str, now: Millis) -> Result<Frame, Refusal> {
        match self {
            Adapter::Bitstamp => bitstamp::parse(text).map_err(Refusal::Message),
            Adapter::Coinbase(books) => books.read(text, now),
            Adapter::Kraken(books) => books.read(text, now),
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
            held: BTreeMap::new(),
            refusals: Throttle::default(),
            clock_alerts: Throttle::default(),
        }
    }

    /// Connects and relays until the connection fails, then retries after a backoff that doubles
    /// while attempts sequence no fresh quote and resets once one does.
    /// Error cases:
    /// - the sequencer has gone away
    async fn run(mut self) -> anyhow::Result<()> {
        self.seed_from_engine().await?;
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

    /// Seeds `held` with the quote times the engine already holds for this venue, so after a
    /// restart a quote the engine will ignore never counts as the venue quoting.
    /// Error cases:
    /// - the sequencer has gone away
    async fn seed_from_engine(&mut self) -> anyhow::Result<()> {
        let (reply, held) = oneshot::channel();
        let source = self.conn.venue.name();
        let markets: Vec<MarketId> = self.markets.values().copied().collect();
        let query: Query = Box::new(move |engine| {
            let quotes = markets
                .iter()
                .filter_map(|market| Some((*market, engine.markets().get(market)?.quotes.get(source)?.ts)));
            let _ = reply.send(quotes.collect());
        });
        self.tx.send(Request::Query(query)).await.map_err(|_| anyhow!("sequencer gone"))?;
        self.held = held.await.context("sequencer gone")?;
        Ok(())
    }

    /// One connection: connect, subscribe, then read messages and sequence one quote per market
    /// every `timing.interval` (see `select`). A quote no older than `timing.stale_after` when sequenced
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

        let mut pending: BTreeMap<MarketId, VecDeque<SourceQuote>> = BTreeMap::new();
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
                    for (market, quotes) in take(&mut pending) {
                        let lead = quotes.iter().map(|quote| quote.ts - now).max();
                        if let Some(lead) = lead.filter(|lead| *lead > MAX_QUOTE_LEAD_MS) {
                            self.clock_ahead(market, lead);
                        }
                        let Some(quote) = select(&quotes, now) else { continue };
                        // As the engine judges it: it takes the quote in place of the one it holds
                        // unless that one is newer and already reached, and counts it once its time
                        // is reached and while it is no older than the bound.
                        let held = self.held.entry(market).or_insert(quote.ts);
                        let taken = quote.ts >= *held || *held > now;
                        if taken {
                            *held = quote.ts;
                        }
                        let fresh = taken && quote.ts <= now && now - quote.ts <= stale_after;
                        let cmd = Command::Prices { market, quotes: vec![quote] };
                        let request = Request::Submit { cmd, reply: None };
                        self.tx.send(request).await.map_err(|_| anyhow!("sequencer gone"))?;
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
        pending: &mut BTreeMap<MarketId, VecDeque<SourceQuote>>,
    ) -> anyhow::Result<()> {
        let now = now_ms();
        let books = match adapter.read(text, now) {
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
            match to_quote(self.conn.venue, &self.markets, book, now) {
                Ok((market, quote)) => {
                    push_pending(pending.entry(market).or_default(), quote);
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
    /// of the host clock, past the `MAX_QUOTE_LEAD_MS` the engine accepts, so it was held back: the
    /// venue's clock or the host's is wrong.
    ///
    /// TODO: the sequencer stamps with the untrusted host clock. One set back by more than
    /// `MAX_QUOTE_LEAD_MS` holds back every quote, and the engine keeps pricing from the quotes it
    /// holds, judged against a sequenced time that a stalled host clock also stalls; the trusted
    /// time bound closes this.
    fn clock_ahead(&mut self, market: MarketId, lead_ms: Millis) {
        if let Some(suppressed) = self.clock_alerts.ready(Instant::now()) {
            let venue = self.conn.venue.name();
            warn!(
                alert = "venue_clock_ahead",
                venue, market, lead_ms, suppressed, "venue time ahead of the host clock"
            );
        }
    }

    /// Logs a refused message, alerting at most once per `REALERT_AFTER`.
    fn refuse(&mut self, reason: &str, text: &str) {
        let venue = self.conn.venue.name();
        let Some(suppressed) = self.refusals.ready(Instant::now()) else {
            debug!(venue, reason, "venue message refused");
            return;
        };
        let excerpt: String = text.chars().take(EXCERPT_CHARS).collect();
        warn!(alert = "venue_message_refused", venue, reason, suppressed, excerpt, "venue message refused");
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

/// Adds a quote received for a market, dropping the oldest once `MAX_PENDING` are held.
pub(super) fn push_pending(quotes: &mut VecDeque<SourceQuote>, quote: SourceQuote) {
    if quotes.len() == MAX_PENDING {
        quotes.pop_front();
    }
    quotes.push_back(quote);
}

/// The quote to sequence for a market from those received since the last tick: the newest the
/// engine can use at host time `now`; when every one is dated after it, the newest the engine still
/// takes, at most `MAX_QUOTE_LEAD_MS` ahead, which it skips until the sequenced time reaches it;
/// and none when every one is further ahead, since the engine would refuse it. A host clock a
/// little behind the venue then still prices from the venue.
pub(super) fn select(quotes: &VecDeque<SourceQuote>, now: Millis) -> Option<SourceQuote> {
    let newest_by = |limit: Millis| quotes.iter().filter(|quote| quote.ts <= limit).max_by_key(|quote| quote.ts);
    newest_by(now).or_else(|| newest_by(now.saturating_add(MAX_QUOTE_LEAD_MS))).cloned()
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
