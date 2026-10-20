//! The quotes a venue task holds between sequencing ticks, and the rule that judges whether the
//! quote the engine holds counts as the venue quoting.

use std::collections::BTreeMap;
use std::mem::replace;

use engine_core::price::{MAX_QUOTE_LEAD_MS, SourceQuote};
use engine_core::types::Millis;

/// One market's quotes received and not yet sequenced: the newest the host clock had reached when
/// it arrived, and those dated ahead of it, kept by venue time until the clock reaches them. A host
/// clock a little behind the venue then still prices from the venue, a moment late, rather than
/// handing the engine quotes it cannot use yet. Quotes ahead are kept one per venue millisecond and
/// at most `MAX_QUOTE_LEAD_MS` ahead, which bounds how many are held.
#[derive(Debug, Default)]
pub(super) struct Pending {
    reached: Option<SourceQuote>,
    ahead: BTreeMap<Millis, SourceQuote>,
}

impl Pending {
    /// Adds a quote received at host time `now`. Of two with the same time, the later one is kept.
    /// Error cases:
    /// - the quote is dated more than `MAX_QUOTE_LEAD_MS` ahead of `now`, which the engine
    ///   refuses, so it is dropped; the error is its lead
    pub(super) fn add(&mut self, quote: SourceQuote, now: Millis) -> Result<(), Millis> {
        let lead = quote.ts.saturating_sub(now);
        if lead > MAX_QUOTE_LEAD_MS {
            return Err(lead);
        }
        if lead > 0 {
            self.ahead.insert(quote.ts, quote);
        } else if self.reached.as_ref().is_none_or(|reached| quote.ts >= reached.ts) {
            self.reached = Some(quote);
        }
        Ok(())
    }

    /// Takes the newest quote the host clock has reached at `now`, keeping those still ahead.
    pub(super) fn take(&mut self, now: Millis) -> Option<SourceQuote> {
        let ahead = self.ahead.split_off(&now.saturating_add(1));
        let caught_up = replace(&mut self.ahead, ahead).pop_last().map(|(_, quote)| quote);
        // A quote that arrived already reached came after any ahead one with the same time.
        caught_up.into_iter().chain(self.reached.take()).max_by_key(|quote| quote.ts)
    }
}

/// Whether a quote dated `ts` that the engine holds at sequenced time `at` counts as the venue
/// quoting: its time reached, and no older than `stale_after` or the market's `max_age_ms`.
pub(super) fn counts(ts: Millis, at: Millis, stale_after: Millis, max_age_ms: Millis) -> bool {
    ts <= at && at.saturating_sub(ts) <= stale_after.min(max_age_ms)
}
