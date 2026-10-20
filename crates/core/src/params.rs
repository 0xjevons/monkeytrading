//! Protocol-wide parameters. Changed only by sequenced admin commands, so
//! every change is in the public log.

use chrono::{DateTime, Duration, TimeZone, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use crate::types::Millis;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProtocolParams {
    /// Fee on realised profit, in bps of the gain. Losing closes pay nothing.
    pub profit_fee_bps: i64,
    /// Account close-out when equity / margin requirement falls below this. The
    /// largest losing position is closed first.
    pub closeout_level_bps: i64,
    /// Cap on the sum of the house's absolute net notional across all
    /// markets, in bps of house NAV.
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
pub struct StressLevel {
    /// Multiplier on size-tier spreads, in bps (10_000 = 1x).
    pub size_spread_multiplier_bps: i64,
    /// Spread applied to every fill at this level, regardless of size.
    pub min_spread_bps: i64,
    /// Conditions that raise a market to this level automatically. Any one
    /// met is enough. `None` means operator-only.
    #[serde(default)]
    pub trigger: Option<StressTrigger>,
}

#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct StressTrigger {
    #[serde(default)]
    pub divergence_bps: Option<i64>,
    #[serde(default)]
    pub move_bps: Option<i64>,
    /// House net exposure in the market as a share of its cap.
    #[serde(default)]
    pub utilisation_bps: Option<i64>,
}

/// Markout-based flow scoring. Each fill is marked against the index
/// `horizon_ms` later; the account's score is an EWMA of those markouts in
/// bps (positive means the trader won). Persistent winners on short horizons
/// are picking off stale prices and pay a wider spread.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToxicityParams {
    pub horizon_ms: i64,
    /// Weight of each new markout in the EWMA, in bps.
    pub ewma_weight_bps: i64,
    /// Ascending by `min_score_bps`; the highest tier reached applies.
    pub tiers: Vec<ToxicityTier>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToxicityTier {
    pub min_score_bps: i64,
    pub extra_spread_bps: i64,
}

/// Daily financing rollover, e.g. 22:00 Europe/London.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rollover {
    pub tz: Tz,
    pub hour: u32,
    pub minute: u32,
}

impl Rollover {
    /// The first rollover strictly after `ts`.
    pub fn next_after(&self, ts: Millis) -> Millis {
        let local = DateTime::<Utc>::from_timestamp_millis(ts)
            .expect("timestamp in range")
            .with_timezone(&self.tz);
        let mut date = local.date_naive();
        loop {
            let naive = date.and_hms_opt(self.hour, self.minute, 0).expect("validated time");
            // In a DST gap the wall time doesn't exist; roll at the gap's end.
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

impl ProtocolParams {
    pub fn validate(&self) -> Result<(), String> {
        if !(0..10_000).contains(&self.profit_fee_bps) {
            return Err("profit_fee_bps must be in [0, 10000)".into());
        }
        if !(1..=10_000).contains(&self.closeout_level_bps) {
            return Err("closeout_level_bps must be in [1, 10000]".into());
        }
        if self.stress_levels.is_empty() {
            return Err("stress_levels needs at least the normal level".into());
        }
        if self.stress_levels[0].trigger.is_some() {
            return Err("stress level 0 is the baseline and takes no trigger".into());
        }
        if !(1..=10_000).contains(&self.toxicity.ewma_weight_bps) {
            return Err("toxicity.ewma_weight_bps must be in [1, 10000]".into());
        }
        if self.toxicity.tiers.windows(2).any(|w| w[0].min_score_bps >= w[1].min_score_bps) {
            return Err("toxicity tiers must be ascending".into());
        }
        if self.rollover.hour > 23 || self.rollover.minute > 59 {
            return Err("invalid rollover time".into());
        }
        Ok(())
    }

    pub fn toxicity_spread_bps(&self, score_bps: i64) -> i64 {
        self.toxicity
            .tiers
            .iter()
            .rev()
            .find(|t| score_bps >= t.min_score_bps)
            .map_or(0, |t| t.extra_spread_bps)
    }
}
