//! Reference index. The house quotes around the median of several
//! independent venues. Quotes too old relative to the sequenced time are
//! dropped, and if too few venues remain, or the fresh ones disagree too
//! much, the market has no price and nothing fills. This staleness guard
//! replaces queue-plus-jitter: a fast feed means there is no stale price to
//! pick off.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::market::PriceRules;
use crate::types::Millis;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceQuote {
    pub source: String,
    pub mid: Fixed,
    /// Venue timestamp of the quote.
    pub ts: Millis,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Index {
    pub mid: Fixed,
    pub divergence_bps: i64,
    pub sources: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PriceError {
    #[error("only {fresh} fresh sources, need {needed}")]
    Stale { fresh: usize, needed: usize },
    #[error("sources diverge by {divergence_bps}bps")]
    Divergent { divergence_bps: i64 },
}

pub fn compute_index(
    quotes: &BTreeMap<String, SourceQuote>,
    rules: &PriceRules,
    now: Millis,
) -> Result<Index, PriceError> {
    let mut mids: Vec<Fixed> = quotes
        .values()
        .filter(|q| q.mid.is_positive() && q.ts <= now && now - q.ts <= rules.max_age_ms)
        .map(|q| q.mid)
        .collect();
    if mids.len() < rules.min_sources {
        return Err(PriceError::Stale { fresh: mids.len(), needed: rules.min_sources });
    }
    mids.sort();
    let n = mids.len();
    let mid = if n % 2 == 1 { mids[n / 2] } else { (mids[n / 2 - 1] + mids[n / 2]).div_int(2) };
    let divergence_bps = (mids[n - 1] - mids[0]).bps_of(mid);
    if divergence_bps > rules.max_divergence_bps {
        return Err(PriceError::Divergent { divergence_bps });
    }
    Ok(Index { mid, divergence_bps, sources: n })
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULES: PriceRules = PriceRules { max_age_ms: 500, max_divergence_bps: 20, min_sources: 2 };

    fn quotes(qs: &[(&str, &str, Millis)]) -> BTreeMap<String, SourceQuote> {
        qs.iter()
            .map(|(s, m, ts)| (s.to_string(), SourceQuote { source: s.to_string(), mid: m.parse().unwrap(), ts: *ts }))
            .collect()
    }

    #[test]
    fn median_of_fresh_sources() {
        let q = quotes(&[("a", "100", 1_000), ("b", "100.1", 1_000), ("c", "100.05", 1_000)]);
        let idx = compute_index(&q, &RULES, 1_200).unwrap();
        assert_eq!(idx.mid, "100.05".parse().unwrap());
        assert_eq!(idx.sources, 3);
        assert_eq!(idx.divergence_bps, 9);
    }

    #[test]
    fn stale_sources_are_dropped() {
        let q = quotes(&[("a", "100", 1_000), ("b", "100.1", 100)]);
        assert_eq!(compute_index(&q, &RULES, 1_000), Err(PriceError::Stale { fresh: 1, needed: 2 }));
    }

    #[test]
    fn divergent_sources_mean_no_price() {
        let q = quotes(&[("a", "100", 1_000), ("b", "101", 1_000)]);
        assert!(matches!(compute_index(&q, &RULES, 1_000), Err(PriceError::Divergent { .. })));
    }
}
