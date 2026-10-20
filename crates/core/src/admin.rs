//! Operator commands and stress levels.
//!
//! Every admin action is a sequenced command, so the public log shows each parameter change, each
//! listing, and each stress override. The operator can raise a market's stress level but never
//! push it below what the automatic triggers set.

use crate::Fixed;
use crate::command::AdminCommand;
use crate::engine::{Engine, MarketState};
use crate::event::Event;
use crate::params::{MAX_BENCHMARK_BPS, StressLevel};
use crate::protocol::ProtocolVersion;
use crate::types::MarketId;

impl Engine {
    /// Applies an operator command.
    /// Error cases:
    /// - `SetParams` with parameters that fail `ProtocolParams::validate`
    /// - `UpsertMarket` with a spec that fails `MarketSpec::validate`
    /// - `SetBenchmarkRate` outside ±10000 bps a year
    /// - `SetStressFloor` before parameters are set, at a level that does not exist, or for an
    ///   unknown market
    /// - `SetReduceOnly` for an unknown market
    /// - `SetClientAuth` once client auth is recorded
    /// - `ActivateProtocol` for a version no newer than the one in force, or for `V3` or later
    ///   before client auth is recorded
    pub(crate) fn admin(&mut self, cmd: &AdminCommand, events: &mut Vec<Event>) -> Result<(), String> {
        match cmd {
            AdminCommand::SetParams(params) => {
                params.validate()?;
                self.next_rollover = Some(params.rollover.next_after(self.now));
                self.params = Some(params.clone());
                self.restress_all(events);
            }
            AdminCommand::UpsertMarket(spec) => {
                spec.validate()?;
                match self.markets.get_mut(&spec.id) {
                    Some(market) => market.spec = spec.clone(),
                    None => {
                        self.markets.insert(spec.id, MarketState::new(spec.clone()));
                    }
                }
                self.refresh_session(spec.id, events);
            }
            AdminCommand::SetBenchmarkRate { benchmark, annual_bps } => {
                if !(-MAX_BENCHMARK_BPS..=MAX_BENCHMARK_BPS).contains(annual_bps) {
                    return Err(format!("benchmark rate {annual_bps}bps out of range"));
                }
                self.benchmarks.insert(benchmark.clone(), *annual_bps);
            }
            AdminCommand::SetStressFloor { market, level } => {
                let levels = self.params.as_ref().ok_or("protocol params not set")?.stress_levels.len();
                if *level >= levels {
                    return Err(format!("stress level {level} does not exist"));
                }
                match market {
                    None => self.global_stress_floor = *level,
                    Some(id) => self.markets.get_mut(id).ok_or("unknown market")?.stress.floor = *level,
                }
                self.restress_all(events);
            }
            AdminCommand::SetReduceOnly { market, reduce_only } => {
                self.markets.get_mut(market).ok_or("unknown market")?.reduce_only = *reduce_only;
            }
            AdminCommand::SetClientAuth(auth) => {
                if self.client_auth.is_some() {
                    return Err("client auth is already recorded".into());
                }
                self.client_auth = Some(auth.clone());
            }
            AdminCommand::ActivateProtocol { version } => {
                if *version <= self.protocol_version {
                    return Err(format!("protocol {version:?} is not newer than {:?}", self.protocol_version));
                }
                if *version >= ProtocolVersion::V3 && self.client_auth.is_none() {
                    return Err(format!("protocol {version:?} needs client auth recorded first"));
                }
                self.protocol_version = *version;
            }
        }
        Ok(())
    }

    /// Recomputes a market's automatic stress level from the latest index, then its effective
    /// level. A raised automatic level steps down only after the cooldown.
    pub(crate) fn update_stress(
        &mut self,
        market_id: MarketId,
        mid: Fixed,
        divergence_bps: i64,
        events: &mut Vec<Event>,
    ) {
        let nav = self.house_nav();
        let now = self.now;
        let global_floor = self.global_stress_floor;
        let params = self.params.as_ref().expect("callers check params are set");
        let market = self.markets.get_mut(&market_id).expect("callers pass a listed market");

        let move_bps = match market.stress.move_ref {
            Some((t, p)) if now - t < params.stress_move_window_ms => (mid - p).abs().bps_of(p),
            _ => {
                market.stress.move_ref = Some((now, mid));
                0
            }
        };
        let cap = nav.mul_bps(market.spec.house_cap_bps);
        let util_bps = if cap.is_positive() { market.client_qty.abs().mul(mid).bps_of(cap) } else { i64::MAX };
        let target = auto_stress_target(&params.stress_levels, divergence_bps, move_bps, util_bps);

        let stress = &mut market.stress;
        if target > stress.auto || (target < stress.auto && now - stress.auto_since >= params.stress_cooldown_ms) {
            stress.auto = target;
            stress.auto_since = now;
        }
        let max_level = params.stress_levels.len() - 1;
        set_stress_level(market_id, market, global_floor, max_level, events);
    }

    /// Recomputes every market's effective stress level after a floor or parameter change.
    fn restress_all(&mut self, events: &mut Vec<Event>) {
        let Some(params) = &self.params else { return };
        let max_level = params.stress_levels.len() - 1;
        let global_floor = self.global_stress_floor;
        for (market_id, market) in &mut self.markets {
            set_stress_level(*market_id, market, global_floor, max_level, events);
        }
    }
}

/// The highest stress level whose trigger any current reading meets, or 0.
fn auto_stress_target(levels: &[StressLevel], divergence_bps: i64, move_bps: i64, util_bps: i64) -> usize {
    levels
        .iter()
        .enumerate()
        .skip(1)
        .rev()
        .find(|(_, level)| {
            level.trigger.as_ref().is_some_and(|t| {
                t.divergence_bps.is_some_and(|x| divergence_bps >= x)
                    || t.move_bps.is_some_and(|x| move_bps >= x)
                    || t.utilisation_bps.is_some_and(|x| util_bps >= x)
            })
        })
        .map_or(0, |(i, _)| i)
}

/// Sets a market's effective stress level to the highest of its automatic level and both floors,
/// capped at the top level, emitting a change.
fn set_stress_level(
    market_id: MarketId,
    market: &mut MarketState,
    global_floor: usize,
    max_level: usize,
    events: &mut Vec<Event>,
) {
    let stress = &mut market.stress;
    let level = stress.auto.max(stress.floor).max(global_floor).min(max_level);
    if level != stress.level {
        stress.level = level;
        events.push(Event::StressChanged { market: market_id, level });
    }
}
