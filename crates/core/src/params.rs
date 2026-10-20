//! Protocol-wide parameters. Changed only by sequenced admin commands, so every change is in the
//! public log.

use chrono::{DateTime, Duration, TimeZone, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use crate::types::Millis;

/// The most any per-trade rate may be: 100%.
const MAX_RATE_BPS: i64 = 10_000;
/// The most a multiplier or house cap may be: ten times.
const MAX_MULTIPLE_BPS: i64 = 100_000;
/// The longest markout horizon: one hour. A horizon near `i64::MAX` overflows a markout's due time,
/// and markouts are marked in fill order, so one queued under a long horizon holds back every later
/// one once the horizon is lowered; the cap bounds that wait.
const MAX_HORIZON_MS: i64 = 3_600_000;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolParams {
    /// Fee on realised profit, in bps of the gain. Losing closes pay nothing.
    pub profit_fee_bps: i64,
    /// Account close-out when equity / margin requirement falls below this. The largest
    /// losing position is closed first.
    pub closeout_level_bps: i64,
    /// Cap on the sum of the house's absolute net notional across all markets, in bps of house NAV.
    pub house_total_cap_bps: i64,
    /// Index 0 is normal conditions. Higher levels widen spreads.
    pub stress_levels: Vec<StressLevel>,
    /// Window over which the stress engine measures a market's price move.
    pub stress_move_window_ms: i64,
    /// Minimum time at a raised stress level before it can step back down.
    pub stress_cooldown_ms: i64,
    pub toxicity: ToxicityParams,
    pub rollover: Rollover,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StressLevel {
    /// Multiplier on size-tier spreads, in bps (10_000 = 1x).
    pub size_spread_multiplier_bps: i64,
    /// Spread applied to every fill at this level, regardless of size.
    pub min_spread_bps: i64,
    /// Conditions that raise a market to this level automatically. Any one met is enough. `None`
    /// means operator-only.
    #[serde(default)]
    pub trigger: Option<StressTrigger>,
}

#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StressTrigger {
    #[serde(default)]
    pub divergence_bps: Option<i64>,
    #[serde(default)]
    pub move_bps: Option<i64>,
    /// House net exposure in the market as a share of its cap.
    #[serde(default)]
    pub utilisation_bps: Option<i64>,
}

/// Markout-based flow scoring. Each fill is marked against the index `horizon_ms` later; the
/// account's score is an EWMA of those markouts in bps (positive means the trader won). Persistent
/// winners on short horizons are picking off stale prices and pay a wider spread.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToxicityParams {
    pub horizon_ms: i64,
    /// Weight of each new markout in the EWMA, in bps.
    pub ewma_weight_bps: i64,
    /// Ascending by `min_score_bps`; the highest tier reached applies.
    pub tiers: Vec<ToxicityTier>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToxicityTier {
    pub min_score_bps: i64,
    pub extra_spread_bps: i64,
}

/// Daily financing rollover, e.g. 22:00 Europe/London.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rollover {
    pub tz: Tz,
    pub hour: u32,
    pub minute: u32,
}

impl ProtocolParams {
    /// Checks every parameter is in range.
    /// Error cases:
    /// - a profit fee outside 0..10000 bps, or a close-out level outside 1..=10000 bps
    /// - a total house cap outside 1..=100000 bps
    /// - no stress levels, a trigger on level 0, a multiplier outside 0..=100000 bps, a minimum
    ///   spread outside 0..=10000 bps, or a negative trigger threshold
    /// - a non-positive move window, or a negative cooldown
    /// - a toxicity horizon outside 1..=3600000 ms, an EWMA weight outside 1..=10000 bps, tiers
    ///   not strictly ascending, or an extra spread outside 0..=10000 bps
    /// - a rollover time that does not exist (hour above 23 or minute above 59)
    pub fn validate(&self) -> Result<(), String> {
        let rate = 0..=MAX_RATE_BPS;
        if !(0..MAX_RATE_BPS).contains(&self.profit_fee_bps) {
            return Err("profit_fee_bps must be in 0..10000".into());
        }
        if !(1..=MAX_RATE_BPS).contains(&self.closeout_level_bps) {
            return Err("closeout_level_bps must be in 1..=10000".into());
        }
        if !(1..=MAX_MULTIPLE_BPS).contains(&self.house_total_cap_bps) {
            return Err("house_total_cap_bps must be in 1..=100000".into());
        }
        let Some(baseline) = self.stress_levels.first() else {
            return Err("stress_levels needs at least the normal level".into());
        };
        if baseline.trigger.is_some() {
            return Err("stress level 0 is the baseline and takes no trigger".into());
        }
        let bad_level = |l: &StressLevel| {
            !(0..=MAX_MULTIPLE_BPS).contains(&l.size_spread_multiplier_bps)
                || !rate.contains(&l.min_spread_bps)
                || l.trigger
                    .as_ref()
                    .is_some_and(|t| [t.divergence_bps, t.move_bps, t.utilisation_bps].iter().flatten().any(|x| *x < 0))
        };
        if self.stress_levels.iter().any(bad_level) {
            return Err("stress level out of range".into());
        }
        if self.stress_move_window_ms <= 0 || self.stress_cooldown_ms < 0 {
            return Err("stress_move_window_ms must be > 0 and stress_cooldown_ms >= 0".into());
        }
        let tox = &self.toxicity;
        if !(1..=MAX_HORIZON_MS).contains(&tox.horizon_ms) || !(1..=MAX_RATE_BPS).contains(&tox.ewma_weight_bps) {
            return Err("toxicity needs horizon_ms in 1..=3600000 and ewma_weight_bps in 1..=10000".into());
        }
        if tox.tiers.windows(2).any(|w| w[0].min_score_bps >= w[1].min_score_bps)
            || tox.tiers.iter().any(|t| !rate.contains(&t.extra_spread_bps))
        {
            return Err("toxicity tiers must be ascending with extra_spread_bps in 0..=10000".into());
        }
        if self.rollover.hour > 23 || self.rollover.minute > 59 {
            return Err("invalid rollover time".into());
        }
        Ok(())
    }

    /// Extra spread for an account with this toxicity score: the highest tier it reaches.
    pub fn toxicity_spread_bps(&self, score_bps: i64) -> i64 {
        self.toxicity.tiers.iter().rev().find(|t| score_bps >= t.min_score_bps).map_or(0, |t| t.extra_spread_bps)
    }
}

impl Rollover {
    /// The first rollover strictly after `ts`. In a DST gap, where the wall time does not exist,
    /// the rollover happens an hour later in wall-clock terms.
    pub fn next_after(&self, ts: Millis) -> Millis {
        // The sequencer stamps from the system clock, which is always inside chrono's range.
        let local = DateTime::<Utc>::from_timestamp_millis(ts).expect("timestamp in range").with_timezone(&self.tz);
        let mut date = local.date_naive();
        loop {
            // `validate` rules out an hour or minute that does not exist.
            let naive = date.and_hms_opt(self.hour, self.minute, 0).expect("validated time");
            let at = self
                .tz
                .from_local_datetime(&naive)
                .earliest()
                .or_else(|| self.tz.from_local_datetime(&(naive + Duration::hours(1))).earliest());
            if let Some(at) = at.map(|t| t.timestamp_millis()).filter(|t| *t > ts) {
                return at;
            }
            date = date.succ_opt().expect("date in range");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn rollover(tz: Tz, hour: u32, minute: u32) -> Rollover {
        Rollover { tz, hour, minute }
    }

    fn utc_ms(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> Millis {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap().timestamp_millis()
    }

    fn params() -> ProtocolParams {
        ProtocolParams {
            profit_fee_bps: 1_000,
            closeout_level_bps: 5_000,
            house_total_cap_bps: 20_000,
            stress_levels: vec![StressLevel { size_spread_multiplier_bps: 10_000, min_spread_bps: 0, trigger: None }],
            stress_move_window_ms: 60_000,
            stress_cooldown_ms: 0,
            toxicity: ToxicityParams { horizon_ms: 1_000, ewma_weight_bps: 5_000, tiers: vec![] },
            rollover: rollover(chrono_tz::Europe::London, 22, 0),
        }
    }

    #[test]
    fn rollover_is_strictly_after_and_follows_local_time() {
        let r = rollover(chrono_tz::Europe::London, 22, 0);
        // BST: 22:00 London is 21:00 UTC; a timestamp exactly at the rollover moves to the next day.
        assert_eq!(r.next_after(utc_ms(2026, 10, 20, 12, 0)), utc_ms(2026, 10, 20, 21, 0));
        assert_eq!(r.next_after(utc_ms(2026, 10, 20, 21, 0)), utc_ms(2026, 10, 21, 21, 0));
        // After the clocks go back on 25 Oct, 22:00 London is 22:00 UTC.
        assert_eq!(r.next_after(utc_ms(2026, 10, 26, 12, 0)), utc_ms(2026, 10, 26, 22, 0));
    }

    #[test]
    fn rollover_in_a_dst_gap_moves_an_hour_later() {
        // 01:30 on 29 Mar 2026 does not exist in London; the clocks jump 01:00 -> 02:00 (01:00 UTC).
        let r = rollover(chrono_tz::Europe::London, 1, 30);
        assert_eq!(r.next_after(utc_ms(2026, 3, 28, 23, 0)), utc_ms(2026, 3, 29, 1, 30));
    }

    #[test]
    fn validation_rejects_out_of_range_values() {
        assert!(params().validate().is_ok());
        let cases: [fn(&mut ProtocolParams); 6] = [
            |p| p.profit_fee_bps = 10_000,
            |p| p.closeout_level_bps = 0,
            |p| p.stress_levels.clear(),
            |p| p.stress_levels[0].trigger = Some(StressTrigger::default()),
            |p| p.stress_levels[0].min_spread_bps = i64::MAX,
            |p| p.toxicity.ewma_weight_bps = 0,
        ];
        for mutate in cases {
            let mut p = params();
            mutate(&mut p);
            assert!(p.validate().is_err(), "{p:?}");
        }
    }

    #[test]
    fn markout_horizon_is_positive_and_at_most_an_hour() {
        let mut p = params();
        // The bounds: 1 ms and one hour.
        for horizon_ms in [1, 3_600_000] {
            p.toxicity.horizon_ms = horizon_ms;
            assert!(p.validate().is_ok(), "{horizon_ms}");
        }
        // A markout needs a positive horizon. Past the cap one could hold back every later markout
        // for longer, and near `i64::MAX` its due time would overflow.
        for horizon_ms in [i64::MIN, -1, 0, 3_600_001, i64::MAX] {
            p.toxicity.horizon_ms = horizon_ms;
            assert!(p.validate().is_err(), "{horizon_ms}");
        }
    }

    #[test]
    fn toxicity_spread_takes_the_highest_tier_reached() {
        let mut p = params();
        p.toxicity.tiers = vec![
            ToxicityTier { min_score_bps: 5, extra_spread_bps: 1 },
            ToxicityTier { min_score_bps: 15, extra_spread_bps: 3 },
        ];
        assert_eq!(p.toxicity_spread_bps(4), 0);
        assert_eq!(p.toxicity_spread_bps(5), 1);
        assert_eq!(p.toxicity_spread_bps(20), 3);
    }
}
