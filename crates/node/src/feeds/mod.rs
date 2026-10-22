//! Price feeds. Each feed handler turns venue data into `Command::Prices`. The engine builds the
//! index (median, staleness, divergence), so the aggregation rules are protocol and the raw venue
//! quotes are in the public log for anyone to check against venue history.
//!
//! `venue` runs one supervised task per venue connection; each venue's adapter (`bitstamp`,
//! `coinbase`, `kraken`) only builds its subscribe messages and reads its messages into books. A
//! new source is an adapter plus a `Venue` variant. The mock feed serves local development.

mod bitstamp;
mod coinbase;
mod kraken;
pub mod venue;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use engine_core::Fixed;
use engine_core::command::Command;
use engine_core::price::SourceQuote;
use engine_core::types::MarketId;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use tokio::sync::mpsc;
use tokio::time::{Instant, interval};

use crate::clock::now_ms;
use crate::config::{FeedProvider, FeedsSection};
use crate::sequencer::Request;

const MOCK_VENUES: [&str; 3] = ["mock-a", "mock-b", "mock-c"];

/// The sources `cfg` feeds each market from: the quote sources the engine is still sent.
pub fn configured_sources(cfg: &FeedsSection) -> BTreeMap<MarketId, BTreeSet<String>> {
    let pairs: Vec<(MarketId, String)> = match cfg.provider {
        FeedProvider::Mock => {
            cfg.mock_mids.iter().flat_map(|m| MOCK_VENUES.map(|venue| (m.market, venue.to_owned()))).collect()
        }
        FeedProvider::Venues => cfg
            .venues
            .iter()
            .flat_map(|venues| &venues.connections)
            .flat_map(|conn| conn.symbols.iter().map(|s| (s.market, conn.venue.name().to_owned())))
            .collect(),
    };
    pairs.into_iter().fold(BTreeMap::new(), |mut sources, (market, venue)| {
        sources.entry(market).or_default().insert(venue);
        sources
    })
}

/// A random walk per market, quoted by three venues that differ by about a bp, until the market's
/// `stop_after`. Returns only when the sequencer has gone away.
pub async fn run_mock(cfg: &FeedsSection, tx: mpsc::Sender<Request>) {
    let mut rng = StdRng::from_entropy();
    let mut mids: Vec<(u32, Fixed, Option<Duration>)> =
        cfg.mock_mids.iter().map(|m| (m.market, m.mid, m.stop_after)).collect();
    let started = Instant::now();
    let mut every = interval(cfg.interval);
    loop {
        every.tick().await;
        let ts = now_ms();
        let updates: Vec<Command> = mids
            .iter_mut()
            .filter(|(_, _, stop_after)| stop_after.is_none_or(|after| started.elapsed() < after))
            .map(|(market, mid, _)| {
                *mid += mid.mul_bps(rng.gen_range(-2..=2));
                let quotes = MOCK_VENUES
                    .iter()
                    .map(|venue| SourceQuote {
                        source: (*venue).to_owned(),
                        mid: *mid + mid.mul_bps(rng.gen_range(-1..=1)),
                        ts,
                    })
                    .collect();
                Command::Prices { market: *market, quotes }
            })
            .collect();
        for cmd in updates {
            if tx.send(Request::Submit { cmd, reply: None }).await.is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests;
