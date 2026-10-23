//! Dated benchmark inputs and financing lookup from V18. Published rates are annual fractions,
//! preserving sub-basis-point precision; the signed batch is installed before catch-up financing.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Datelike, NaiveDate, Weekday};
use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::engine::Engine;
use crate::event::Event;
use crate::protocol::ProtocolVersion;
use crate::types::{MarketId, Millis};

/// At most this many observations in one signed snapshot.
pub const MAX_BENCHMARK_RATES: usize = 16_384;
pub type BenchmarkHistory = BTreeMap<String, BTreeMap<NaiveDate, Fixed>>;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkRate {
    pub benchmark: String,
    pub effective_date: NaiveDate,
    /// Annual fraction: 0.045 means 4.5%, or 450 bps.
    pub annual_rate: Fixed,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkBatch {
    /// The rollover-local date through which the worker attempted to fetch publisher histories.
    pub as_of: NaiveDate,
    /// Includes failed fetches: their previous rate still applies, or no financing if none exists.
    pub checked: BTreeSet<String>,
    pub rates: Vec<BenchmarkRate>,
}

impl Engine {
    /// The rollover-local calendar date at `ts`, or UTC before parameters are recorded.
    pub fn benchmark_date(&self, ts: Millis) -> NaiveDate {
        let utc = DateTime::from_timestamp_millis(ts).expect("engine timestamps fit chrono");
        self.params.as_ref().map_or(utc.date_naive(), |p| utc.with_timezone(&p.rollover.tz).date_naive())
    }

    /// The next rollover, for a publisher worker to request history covering downtime.
    pub fn next_rollover(&self) -> Option<Millis> {
        self.next_rollover
    }

    /// The publisher history retained for the next rollover, including its preceding observation.
    pub fn benchmark_history(&self) -> &BenchmarkHistory {
        &self.benchmark_history
    }

    /// Validates the whole snapshot before changing any rate. An empty history records a failed
    /// fetch attempt and deliberately retains the missing/stale financing policy.
    /// Error cases:
    /// - before V18, wrong as-of date, too many rates, an unchecked market, or invalid/duplicate data
    pub(crate) fn set_benchmark_rates(&mut self, batch: &BenchmarkBatch) -> Result<(), String> {
        if self.protocol_version < ProtocolVersion::V18 {
            return Err("dated benchmark rates need protocol V18".into());
        }
        if batch.as_of != self.benchmark_date(self.now) {
            return Err("benchmark snapshot must cover the current rollover-local date".into());
        }
        if batch.rates.len() > MAX_BENCHMARK_RATES || batch.checked.len() > 256 {
            return Err("benchmark snapshot is too large".into());
        }
        if self.markets.values().any(|m| !batch.checked.contains(&m.spec.financing.benchmark)) {
            return Err("benchmark snapshot must check every listed market's benchmark".into());
        }
        let mut seen = BTreeSet::new();
        for rate in &batch.rates {
            if rate.benchmark.is_empty() || rate.benchmark.len() > 64 || !batch.checked.contains(&rate.benchmark) {
                return Err("invalid or unchecked benchmark name".into());
            }
            if rate.annual_rate < -Fixed::ONE || rate.annual_rate > Fixed::ONE {
                return Err("benchmark annual rate must be within +/-100%".into());
            }
            if rate.effective_date > batch.as_of || rate.effective_date.year() < 1970 {
                return Err("benchmark effective date must be between 1970 and the snapshot date".into());
            }
            if !seen.insert((&rate.benchmark, rate.effective_date)) {
                return Err("duplicate benchmark effective date".into());
            }
        }
        for rate in &batch.rates {
            self.benchmark_history
                .entry(rate.benchmark.clone())
                .or_default()
                .insert(rate.effective_date, rate.annual_rate);
        }
        if let Some(at) = self.next_rollover {
            self.prune_benchmark_history(self.benchmark_date(at));
        }
        Ok(())
    }

    /// The last published observation effective on or before a rollover's local date.
    pub(crate) fn dated_benchmark(&self, name: &str, date: NaiveDate) -> Option<(NaiveDate, Fixed)> {
        self.benchmark_history.get(name)?.range(..=date).next_back().map(|(day, rate)| (*day, *rate))
    }

    /// Records missing and stale observations, and their recovery, for every required benchmark.
    pub(crate) fn benchmark_status(&self, date: NaiveDate, events: &mut Vec<Event>) {
        let mut markets = BTreeMap::<&str, Vec<MarketId>>::new();
        for (id, market) in self.markets.iter() {
            markets.entry(&market.spec.financing.benchmark).or_default().push(*id);
        }
        for (benchmark, markets) in markets {
            match self.dated_benchmark(benchmark, date) {
                Some((effective_date, _)) => events.push(Event::BenchmarkRateObserved {
                    benchmark: benchmark.into(),
                    effective_date,
                    rollover_date: date,
                    stale: business_days(effective_date, date) > 5,
                }),
                None if self.benchmarks.contains_key(benchmark) => {
                    events.push(Event::BenchmarkRateUndated { benchmark: benchmark.into() })
                }
                None => events.push(Event::BenchmarkRateMissing { benchmark: benchmark.into(), markets }),
            }
        }
    }

    /// Drops observations superseded before the next rollover, retaining its predecessor.
    pub(crate) fn prune_benchmark_history(&mut self, date: NaiveDate) {
        for history in self.benchmark_history.values_mut() {
            if let Some((&last, _)) = history.range(..=date).next_back() {
                *history = history.split_off(&last);
            }
        }
    }
}

/// Weekdays strictly after `from`, through `to`; publisher holidays do not advance observations.
pub fn business_days(from: NaiveDate, to: NaiveDate) -> i64 {
    let days = (to - from).num_days().max(0);
    let weeks = days / 7;
    let remaining = (1..=days % 7)
        .filter(|offset| !matches!((from + chrono::Duration::days(*offset)).weekday(), Weekday::Sat | Weekday::Sun))
        .count();
    weeks * 5 + remaining as i64
}

/// The state the operator worker reads before fetching a restart's missing history.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BenchmarkState {
    pub next_rollover: Option<Millis>,
    pub rollover: Option<crate::params::Rollover>,
    pub required: BTreeSet<String>,
    pub history: BenchmarkHistory,
}

impl Engine {
    /// The public benchmark schedule and retained observations.
    pub fn benchmark_state(&self) -> BenchmarkState {
        BenchmarkState {
            next_rollover: self.next_rollover,
            rollover: self.params.as_ref().map(|p| p.rollover.clone()),
            required: self.markets.values().map(|m| m.spec.financing.benchmark.clone()).collect(),
            history: self.benchmark_history.clone(),
        }
    }
}

impl BenchmarkState {
    /// The rollover-local date at `ts`.
    pub fn date_at(&self, ts: Millis) -> NaiveDate {
        let utc = DateTime::from_timestamp_millis(ts).expect("timestamps fit chrono");
        self.rollover.as_ref().map_or(utc.date_naive(), |r| utc.with_timezone(&r.tz).date_naive())
    }
}
