//! Market listings. Every field here is protocol: listings enter the engine
//! as sequenced admin commands, so the public log records exactly which rules
//! applied to every fill.

use serde::{Deserialize, Serialize};

use crate::session::Schedule;
use crate::types::{AssetClass, MarketId};
use crate::Fixed;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MarketSpec {
    pub id: MarketId,
    pub symbol: String,
    pub asset_class: AssetClass,
    pub schedule: Schedule,
    /// Margin bands by position notional (USD):
    /// each band's rate applies to the slice of notional inside it.
    pub margin_tiers: Vec<Tier>,
    /// Size spread bands. Spread is charged only on the slice of a trade that
    /// grows the account's absolute position notional through each band, so
    /// normal size pays zero and splitting a large order doesn't avoid it.
    pub spread_tiers: Vec<Tier>,
    pub price_rules: PriceRules,
    /// Largest adverse move from the price the client saw that a market
    /// order will still fill at. Beyond this the order is declined.
    pub max_slippage_bps: i64,
    pub financing: FinancingSpec,
    /// Cap on the house's net notional in this market, in bps of house NAV.
    pub house_cap_bps: i64,
    #[serde(default)]
    pub min_order_notional: Fixed,
}

/// A band in a tier table. `up_to: None` means unbounded and must be last.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tier {
    #[serde(default)]
    pub up_to: Option<Fixed>,
    pub bps: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriceRules {
    /// A source quote older than this (relative to the sequenced time) is ignored.
    pub max_age_ms: i64,
    /// If fresh sources disagree by more than this the market has no price.
    pub max_divergence_bps: i64,
    pub min_sources: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinancingSpec {
    /// Key into the benchmark rate table, e.g. "SOFR", "SONIA", "ESTR".
    pub benchmark: String,
    /// Annual markup over the benchmark: longs pay benchmark + markup,
    /// shorts receive benchmark - markup (and pay if that is negative).
    pub markup_bps: i64,
    /// 360 or 365.
    pub day_basis: i64,
}

impl MarketSpec {
    pub fn validate(&self) -> Result<(), String> {
        self.schedule.validate()?;
        validate_tiers("margin_tiers", &self.margin_tiers)?;
        validate_tiers("spread_tiers", &self.spread_tiers)?;
        if self.margin_tiers.iter().any(|t| t.bps <= 0) {
            return Err("margin tier rates must be positive".into());
        }
        if self.price_rules.min_sources == 0 {
            return Err("price_rules.min_sources must be at least 1".into());
        }
        if self.max_slippage_bps < 0 || self.house_cap_bps <= 0 {
            return Err("max_slippage_bps must be >= 0 and house_cap_bps > 0".into());
        }
        if !matches!(self.financing.day_basis, 360 | 365) {
            return Err("financing.day_basis must be 360 or 365".into());
        }
        Ok(())
    }

    pub fn margin_for(&self, notional: Fixed) -> Fixed {
        staircase(&self.margin_tiers, Fixed::ZERO, notional.abs())
    }
}

fn validate_tiers(name: &str, tiers: &[Tier]) -> Result<(), String> {
    let Some(last) = tiers.last() else {
        return Err(format!("{name} is empty"));
    };
    if last.up_to.is_some() {
        return Err(format!("{name}: last tier must be unbounded"));
    }
    let mut prev = Fixed::ZERO;
    for t in &tiers[..tiers.len() - 1] {
        let up_to = t.up_to.ok_or_else(|| format!("{name}: only the last tier may be unbounded"))?;
        if up_to <= prev {
            return Err(format!("{name}: bands must be strictly increasing"));
        }
        prev = up_to;
    }
    if tiers.iter().any(|t| t.bps < 0) {
        return Err(format!("{name}: negative rate"));
    }
    Ok(())
}

/// Cost of moving from notional `from` to `to` (`from <= to`) through banded
/// rates: each band's bps applies to the slice of `[from, to]` inside it.
pub fn staircase(tiers: &[Tier], from: Fixed, to: Fixed) -> Fixed {
    if to <= from {
        return Fixed::ZERO;
    }
    let mut cost = Fixed::ZERO;
    let mut lo = Fixed::ZERO;
    for t in tiers {
        let hi = t.up_to;
        let start = from.max(lo);
        let end = match hi {
            Some(hi) => to.min(hi),
            None => to,
        };
        if end > start {
            cost += (end - start).mul_bps(t.bps);
        }
        match hi {
            Some(hi) if hi < to => lo = hi,
            _ => break,
        }
    }
    cost
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
        assert_eq!(staircase(&t, f(0), f(2_000_000)), f(200));
        // 1m..5m at 2bps = 800, 5m..6m at 5bps = 500
        assert_eq!(staircase(&t, f(0), f(6_000_000)), f(1_300));
        assert_eq!(staircase(&t, f(4_000_000), f(6_000_000)), f(700));
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
        assert!(validate_tiers("t", &tiers()).is_ok());
        assert!(validate_tiers("t", &[]).is_err());
        assert!(validate_tiers("t", &[Tier { up_to: Some(f(1)), bps: 1 }]).is_err());
        let backwards = vec![
            Tier { up_to: Some(f(5)), bps: 1 },
            Tier { up_to: Some(f(2)), bps: 1 },
            Tier { up_to: None, bps: 1 },
        ];
        assert!(validate_tiers("t", &backwards).is_err());
    }
}
