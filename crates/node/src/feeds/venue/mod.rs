//! One venue's WebSocket feed. A task connects, subscribes, and sequences the venue's top-of-book
//! mids as `Command::Prices`, each stamped with the venue's own timestamp. It returns only when the
//! sequencer has gone away: a failed connection is retried inside the task with backoff, and while
//! it is down the venue's last quotes age out in the engine, which prices from the other venues.

mod quotes;
use quotes::{Pending, counts};
mod setup;
use setup::next_attempt;
pub use setup::tls_config;
mod validation;
pub(super) use validation::{check_size, check_time, to_quote};

use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;
use std::mem::take;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use engine_core::Fixed;
use engine_core::command::Command;
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

/// After a refused-message or clock alert, the same alert from the same venue waits this long, so
/// a venue that changes its format raises one alert a minute, not one per message.
const REALERT_AFTER: Duration = Duration::from_secs(60);
/// How much of a refused message its log line keeps.
const EXCERPT_CHARS: usize = 200;

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
    /// Markets the current connection has sequenced quotes for that no confirmation covers yet.
    unconfirmed: BTreeSet<MarketId>,
    /// The one confirmation awaited, and the markets it covers (see `confirm`).
    confirming: Option<(oneshot::Receiver<Vec<MarketId>>, BTreeSet<MarketId>)>,
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
            unconfirmed: BTreeSet::new(),
            confirming: None,
            refusals: Throttle::default(),
            clock_alerts: Throttle::default(),
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
            self.settle().await;
            if self.tx.is_closed() {
                return Err(error.context("sequencer gone"));
            }
            let (wait, next) = next_attempt(backoff, self.quoted, &self.timing);
            self.dropped(&error, wait);
            sleep(wait).await;
            backoff = next;
        }
    }

    /// One connection: connect, subscribe, then read messages and sequence one quote per market
    /// every `timing.interval`, the newest the host clock has reached (see `Pending`). Once the
    /// sequencer has applied it, the venue counts as quoting that market while the quote the engine
    /// holds from it is no older than `timing.stale_after` or the market's `max_age_ms` (see
    /// `confirm`).
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

        let mut pending: BTreeMap<MarketId, Pending> = BTreeMap::new();
        let mut flush = interval_at(subscribed + self.timing.interval, self.timing.interval);
        flush.set_missed_tick_behavior(MissedTickBehavior::Delay);
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
                fresh = async {
                    match &mut self.confirming {
                        Some((reply, _)) => reply.await,
                        None => std::future::pending().await,
                    }
                } => {
                    self.confirming = None;
                    fresh.map_err(|_| anyhow!("sequencer gone"))?.into_iter().for_each(|market| self.delivered(market));
                }
                _ = flush.tick() => {
                    let now = now_ms();
                    for (market, quotes) in &mut pending {
                        let Some(quote) = quotes.take(now) else { continue };
                        self.unconfirmed.insert(*market);
                        let cmd = Command::Prices { market: *market, quotes: vec![quote] };
                        let request = Request::Submit { cmd, reply: None };
                        self.tx.send(request).await.map_err(|_| anyhow!("sequencer gone"))?;
                    }
                    if self.confirming.is_none() && !self.unconfirmed.is_empty() {
                        let markets = take(&mut self.unconfirmed);
                        self.confirming = Some((self.confirm(markets.clone()).await?, markets));
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

    /// Asks the sequencer, behind the quotes already sent, for the `markets` where the venue
    /// counts as quoting once they are applied: the engine lists the market, and the quote it holds
    /// from this venue passes `counts` at the time the sequencer would stamp next. Judging what the
    /// engine holds, not what was sent, means a market it does not list never counts, and a quote
    /// a lagging sequencer applies late counts as stale.
    /// Error cases:
    /// - the sequencer has gone away
    async fn confirm(&self, markets: BTreeSet<MarketId>) -> anyhow::Result<oneshot::Receiver<Vec<MarketId>>> {
        let (reply, fresh) = oneshot::channel();
        let source = self.conn.venue.name();
        let stale_after = Millis::try_from(self.timing.stale_after.as_millis()).unwrap_or(Millis::MAX);
        let query: Query = Box::new(move |engine| {
            let at = now_ms().max(engine.now());
            let quoting = |market: &MarketId| {
                let Some(state) = engine.markets().get(market) else { return false };
                let max_age_ms = state.spec.price_rules.max_age_ms;
                state.quotes.get(source).is_some_and(|held| counts(held.ts, at, stale_after, max_age_ms))
            };
            let _ = reply.send(markets.into_iter().filter(quoting).collect());
        });
        self.tx.send(Request::Query(query)).await.map_err(|_| anyhow!("sequencer gone"))?;
        Ok(fresh)
    }

    /// Settles what the ended connection sequenced, so one that quoted just before it ended is not
    /// taken for a failed attempt: one last confirmation covers every market not yet confirmed,
    /// waiting at most `timing.timeout` for the answer.
    async fn settle(&mut self) {
        let mut markets = take(&mut self.unconfirmed);
        markets.extend(self.confirming.take().into_iter().flat_map(|(_, covered)| covered));
        if markets.is_empty() {
            return;
        }
        let confirmed = async { anyhow::Ok(self.confirm(markets).await?.await?) };
        if let Ok(Ok(fresh)) = timeout(self.timing.timeout, confirmed).await {
            fresh.into_iter().for_each(|market| self.delivered(market));
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
        pending: &mut BTreeMap<MarketId, Pending>,
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
                    if let Err(lead_ms) = pending.entry(market).or_default().add(quote, now) {
                        self.clock_ahead(market, lead_ms);
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
    /// or too far ahead of the host clock, or reach the engine late. An outage already alerted
    /// stands for every symbol.
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
    /// of the host clock, past the `MAX_QUOTE_LEAD_MS` the engine accepts, so it was dropped: the
    /// venue's clock or the host's is wrong.
    ///
    /// TODO: the sequencer stamps with the untrusted host clock. One set back by more than
    /// `MAX_QUOTE_LEAD_MS` drops every quote, and the engine keeps pricing from the quotes it
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

#[cfg(test)]
mod tests;
