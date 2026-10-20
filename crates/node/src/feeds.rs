//! Price feeds. Each feed handler turns venue data into `Command::Prices`. The engine builds the
//! index (median, staleness, divergence), so the aggregation rules are protocol and the raw venue
//! quotes are in the public log for anyone to check against venue history.
//!
//! Only a mock feed exists so far. Real adapters (crypto venue WebSockets, FX and equity data
//! vendors) belong here, one task per venue, all sending into the sequencer.

use engine_core::Fixed;
use engine_core::command::Command;
use engine_core::price::SourceQuote;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use tokio::sync::mpsc;
use tokio::time::interval;

use crate::clock::now_ms;
use crate::config::FeedsSection;
use crate::sequencer::Request;

const MOCK_VENUES: [&str; 3] = ["mock-a", "mock-b", "mock-c"];

/// A random walk per market, quoted by three venues that differ by about a bp. Returns only when
/// the sequencer has gone away.
pub async fn run_mock(cfg: &FeedsSection, tx: mpsc::Sender<Request>) {
    let mut rng = StdRng::from_entropy();
    let mut mids: Vec<(u32, Fixed)> = cfg.mock_mids.iter().map(|m| (m.market, m.mid)).collect();
    let mut every = interval(cfg.interval);
    loop {
        every.tick().await;
        let ts = now_ms();
        let updates: Vec<Command> = mids
            .iter_mut()
            .map(|(market, mid)| {
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
