//! Confirming with the engine, once the sequencer has applied a venue's quotes, which markets the
//! venue counts as quoting and which the engine does not list.

use std::collections::BTreeSet;
use std::mem::take;

use anyhow::anyhow;
use engine_core::types::{MarketId, Millis};
use tokio::sync::oneshot;
use tokio::time::{Instant, timeout};
use tracing::{info, warn};

use crate::clock::now_ms;
use crate::feeds::venue::VenueFeed;
use crate::feeds::venue::quotes::counts;
use crate::sequencer::{Query, Request};

/// What the engine says of the markets a confirmation asks about.
#[derive(Debug, Default)]
pub(super) struct Confirmation {
    /// Markets where the quote the engine holds from the venue counts as the venue quoting.
    pub(super) fresh: Vec<MarketId>,
    /// Markets the engine does not list.
    pub(super) unlisted: Vec<MarketId>,
}

impl VenueFeed {
    /// Asks the sequencer, behind the quotes already sent, about `markets` and every market held
    /// back as unlisted. A market counts as quoted when the engine lists it and the quote it holds
    /// from this venue passes `counts` at the time the sequencer would stamp next. Judging what the
    /// engine holds, not what was sent, means a quote a lagging sequencer applies late counts as
    /// stale.
    /// Error cases:
    /// - the sequencer has gone away
    pub(super) async fn confirm(
        &self,
        mut markets: BTreeSet<MarketId>,
    ) -> anyhow::Result<oneshot::Receiver<Confirmation>> {
        markets.extend(&self.unlisted);
        let (reply, answer) = oneshot::channel();
        let source = self.conn.venue.name();
        let stale_after = Millis::try_from(self.timing.stale_after.as_millis()).unwrap_or(Millis::MAX);
        let query: Query = Box::new(move |engine| {
            let at = now_ms().max(engine.now());
            let mut confirmation = Confirmation::default();
            for market in markets {
                let Some(state) = engine.markets().get(&market) else {
                    confirmation.unlisted.push(market);
                    continue;
                };
                let max_age_ms = state.spec.price_rules.max_age_ms;
                if state.quotes.get(source).is_some_and(|held| counts(held.ts, at, stale_after, max_age_ms)) {
                    confirmation.fresh.push(market);
                }
            }
            let _ = reply.send(confirmation);
        });
        self.tx.send(Request::Query(query)).await.map_err(|_| anyhow!("sequencer gone"))?;
        Ok(answer)
    }

    /// Records an answer: the markets quoted, and the markets the engine does not list, alerted
    /// once each and held back from then on. A market held back that the answer leaves out is
    /// listed again, since every confirmation asks about all of them, so its quotes resume with
    /// a full silence window.
    pub(super) fn confirmed(&mut self, confirmation: Confirmation) {
        let venue = self.conn.venue.name();
        let unlisted: BTreeSet<MarketId> = confirmation.unlisted.into_iter().collect();
        for market in self.unlisted.difference(&unlisted).copied().collect::<Vec<_>>() {
            self.unlisted.remove(&market);
            self.last_quote.insert(market, Instant::now());
            let symbol = self.symbol_of(market);
            info!(
                alert_resolved = "venue_market_unlisted",
                venue, symbol, market, "venue market listed, quoting it again"
            );
        }
        for market in unlisted {
            if self.unlisted.insert(market) {
                let symbol = self.symbol_of(market);
                warn!(
                    alert = "venue_market_unlisted",
                    venue, symbol, market, "the engine does not list this market, so its quotes are held back"
                );
            }
        }
        confirmation.fresh.into_iter().for_each(|market| self.delivered(market));
    }

    /// Settles what the ended connection sequenced, so one that quoted just before it ended is not
    /// taken for a failed attempt: one last confirmation covers every market not yet confirmed,
    /// waiting at most `timing.timeout` for the answer.
    pub(super) async fn settle(&mut self) {
        let mut markets = take(&mut self.unconfirmed);
        markets.extend(self.confirming.take().into_iter().flat_map(|(_, covered)| covered));
        if markets.is_empty() {
            return;
        }
        let answered = async { anyhow::Ok(self.confirm(markets).await?.await?) };
        if let Ok(Ok(confirmation)) = timeout(self.timing.timeout, answered).await {
            self.confirmed(confirmation);
        }
    }
}
