//! Market listings. Every field here is protocol: listings enter the engine as sequenced admin
//! commands, so the public log records exactly which rules applied to every fill.

use std::iter::once;

use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::session::Schedule;
use crate::types::{AssetClass, MarketId};

/// The most any per-trade rate may be: 100%.
const MAX_RATE_BPS: i64 = 10_000;
/// The most a house exposure cap may be: ten times house NAV.
const MAX_HOUSE_CAP_BPS: i64 = 100_000;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarketSpec {
    pub id: MarketId,
    pub symbol: String,
    pub asset_class: AssetClass,
    pub schedule: Schedule,
    /// Margin bands by position notional (USD): each band's
    /// rate applies to the slice of notional inside it.
    pub margin_tiers: Vec<Tier>,
    /// Size spread bands. Spread is charged only on the slice of a trade that grows the account's
    /// absolute position notional through each band, so normal size pays zero and splitting a
    /// large order doesn't avoid it.
    pub spread_tiers: Vec<Tier>,
    pub price_rules: PriceRules,
    /// Largest adverse move from the price the client saw that a market order will still fill at.
    /// Beyond this the order is declined.
    pub max_slippage_bps: i64,
    pub financing: FinancingSpec,
    /// Cap on the house's net notional in this market, in bps of house NAV.
    pub house_cap_bps: i64,
    #[serde(default)]
    pub min_order_notional: Fixed,
}

/// A band in a tier table. `up_to: None` means unbounded and must be last.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tier {
    #[serde(default)]
    pub up_to: Option<Fixed>,
    pub bps: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PriceRules {
    /// A source quote older than this (relative to the sequenced time) is ignored.
    pub max_age_ms: i64,
    /// If fresh sources disagree by more than this the market has no price.
    pub max_divergence_bps: i64,
    pub min_sources: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinancingSpec {
    /// Key into the benchmark rate table, e.g. "SOFR", "SONIA", "ESTR".
    pub benchmark: String,
    /// Annual markup over the benchmark: longs pay benchmark + markup, shorts receive
    /// benchmark - markup (and pay if that is negative).
    pub markup_bps: i64,
    /// 360 or 365.
    pub day_basis: i64,
}

impl MarketSpec {
    /// Checks the listing is internally consistent and every rate is in range.
    /// Error cases:
    /// - an empty symbol, or a schedule that can never open
    /// - a tier table that is empty, out of order, or not unbounded at the end
    /// - a margin rate outside 1..=10000 bps, or a spread rate outside 0..=10000 bps
    /// - `min_sources` of zero, a non-positive `max_age_ms`, or a divergence outside 0..=10000 bps
    /// - a slippage bound outside 0..=10000 bps, or a house cap outside 1..=100000 bps
    /// - a financing markup outside ±10000 bps, or a day basis other than 360 or 365
    /// - a minimum order notional below zero or above `Fixed::MAX_INPUT`
    pub fn validate(&self) -> Result<(), String> {
        if self.symbol.is_empty() {
            return Err("symbol is empty".into());
        }
        self.schedule.validate()?;
        validate_tiers("margin_tiers", &self.margin_tiers, 1)?;
        validate_tiers("spread_tiers", &self.spread_tiers, 0)?;
        let rules = &self.price_rules;
        if rules.min_sources == 0 || rules.max_age_ms <= 0 {
            return Err("price_rules needs min_sources >= 1 and max_age_ms > 0".into());
        }
        if !(0..=MAX_RATE_BPS).contains(&rules.max_divergence_bps)
            || !(0..=MAX_RATE_BPS).contains(&self.max_slippage_bps)
        {
            return Err("max_divergence_bps and max_slippage_bps must be in 0..=10000".into());
        }
        if !(1..=MAX_HOUSE_CAP_BPS).contains(&self.house_cap_bps) {
            return Err("house_cap_bps must be in 1..=100000".into());
        }
        if !(-MAX_RATE_BPS..=MAX_RATE_BPS).contains(&self.financing.markup_bps) {
            return Err("financing.markup_bps must be within ±10000".into());
        }
        if !matches!(self.financing.day_basis, 360 | 365) {
            return Err("financing.day_basis must be 360 or 365".into());
        }
        if self.min_order_notional.is_negative() || self.min_order_notional > Fixed::MAX_INPUT {
            return Err("min_order_notional out of range".into());
        }
        Ok(())
    }

    /// Margin required for a position of `notional` (either sign).
    pub fn margin_for(&self, notional: Fixed) -> Fixed {
        staircase(&self.margin_tiers, Fixed::ZERO, notional.abs())
    }
}

/// Checks a tier table: non-empty, strictly increasing bounds, unbounded last band, and every rate
/// in `min_bps..=10000`.
/// Error cases:
/// - an empty table
/// - a bounded last band, or an unbounded band before the last
/// - a bound that does not exceed the previous one
/// - a rate outside `min_bps..=10000`
fn validate_tiers(name: &str, tiers: &[Tier], min_bps: i64) -> Result<(), String> {
    let Some((last, bounded)) = tiers.split_last() else {
        return Err(format!("{name} is empty"));
    };
    if last.up_to.is_some() {
        return Err(format!("{name}: last tier must be unbounded"));
    }
    let bounds: Vec<Fixed> = bounded
        .iter()
        .map(|t| t.up_to.ok_or_else(|| format!("{name}: only the last tier may be unbounded")))
        .collect::<Result<_, _>>()?;
    if bounds.first().is_some_and(|b| !b.is_positive()) || bounds.windows(2).any(|w| w[1] <= w[0]) {
        return Err(format!("{name}: bands must be positive and strictly increasing"));
    }
    if tiers.iter().any(|t| !(min_bps..=MAX_RATE_BPS).contains(&t.bps)) {
        return Err(format!("{name}: rates must be in {min_bps}..=10000 bps"));
    }
    Ok(())
}

/// Cost of moving from notional `from` to `to` (`from <= to`) through banded rates: each band's bps
/// applies to the slice of `[from, to]` inside it.
pub fn staircase(tiers: &[Tier], from: Fixed, to: Fixed) -> Fixed {
    if to <= from {
        return Fixed::ZERO;
    }
    let lows = once(Fixed::ZERO).chain(tiers.iter().filter_map(|t| t.up_to));
    tiers
        .iter()
        .zip(lows)
        .map(|(t, lo)| {
            let start = from.max(lo);
            let end = t.up_to.map_or(to, |hi| to.min(hi));
            if end > start { (end - start).mul_bps(t.bps) } else { Fixed::ZERO }
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(n: i64) -> Fixed {
        Fixed::from_int(n)
    }

    fn tiers() -> Vec<Tier> {
        vec![
            Tier { up_to: Some(f(1_000_000)), bps: 0 },
            Tier { up_to: Some(f(5_000_000)), bps: 2 },
            Tier { up_to: None, bps: 5 },
        ]
    }

    #[test]
    fn staircase_charges_each_slice_at_its_band_rate() {
        let t = tiers();
        assert_eq!(staircase(&t, f(0), f(500_000)), f(0));
        // 1m..2m at 2bps.
        assert_eq!(staircase(&t, f(0), f(2_000_000)), f(200));
        // 1m..5m at 2bps = 800, 5m..6m at 5bps = 500.
        assert_eq!(staircase(&t, f(0), f(6_000_000)), f(1_300));
        assert_eq!(staircase(&t, f(4_000_000), f(6_000_000)), f(700));
        // A shrinking move costs nothing.
        assert_eq!(staircase(&t, f(6_000_000), f(4_000_000)), f(0));
    }

    #[test]
    fn splitting_does_not_avoid_size_spread() {
        let t = tiers();
        let whole = staircase(&t, f(0), f(6_000_000));
        let split = staircase(&t, f(0), f(3_000_000)) + staircase(&t, f(3_000_000), f(6_000_000));
        assert_eq!(whole, split);
    }

    #[test]
    fn tier_validation() {
        assert!(validate_tiers("t", &tiers(), 0).is_ok());
        // A zero rate is fine for spreads but not for margin.
        assert!(validate_tiers("t", &tiers(), 1).is_err());
        assert!(validate_tiers("t", &[], 0).is_err());
        assert!(validate_tiers("t", &[Tier { up_to: Some(f(1)), bps: 1 }], 0).is_err());
        let backwards =
            vec![Tier { up_to: Some(f(5)), bps: 1 }, Tier { up_to: Some(f(2)), bps: 1 }, Tier { up_to: None, bps: 1 }];
        assert!(validate_tiers("t", &backwards, 0).is_err());
        let huge = vec![Tier { up_to: None, bps: i64::MAX }];
        assert!(validate_tiers("t", &huge, 0).is_err());
    }

    #[test]
    fn unknown_tier_fields_are_rejected() {
        assert!(serde_json::from_str::<Tier>(r#"{"up_to":"1","bps":1,"bsp":2}"#).is_err());
    }
}
