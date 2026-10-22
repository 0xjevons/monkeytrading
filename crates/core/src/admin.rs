//! Operator commands and stress levels.
//!
//! Every admin action is a sequenced command, so the public log shows each parameter change, each
//! listing, and each stress override. From `V8` the operator signs its commands (listings,
//! parameters, benchmark rates, stress floors, and reduce-only) once anything is credited, and the
//! log carries each signature; client auth, the operator, and protocol activation stay commands
//! only the node sequences. The operator can raise a market's stress level but never
//! push it below what the automatic triggers set, and from `V8` neither a reduce-only mode nor a
//! spread stops a trade that reduces or closes a position in an open market with a price.

use crate::Fixed;
use crate::command::{AdminCommand, AdminRequest};
use crate::engine::{Engine, MarketState};
use crate::event::{Event, Reject};
use crate::fixed::Rounding;
use crate::market::MarketSpec;
use crate::params::{MAX_BENCHMARK_BPS, StressLevel};
use crate::protocol::ProtocolVersion;
use crate::types::MarketId;

impl Engine {
    /// Applies an operator command. From `V6`, new parameters check every account for close-out, as
    /// the close-out level may rise, and a listing checks every account holding the market, as its
    /// margin table may rise. From `V7`, a listing of a market others convert at also marks it and
    /// them as a price update does.
    /// Error cases:
    /// - `SetParams` with parameters that fail `ProtocolParams::validate`, or from `V8` that fail
    ///   `ProtocolParams::check_closing_spread`, or from `V11` `ProtocolParams::check_notional_weighting`
    /// - `UpsertMarket` with a spec that fails `MarketSpec::validate` or `check_quote_currency`, or with
    ///   dated closures or special sessions before `V4`
    /// - `SetBenchmarkRate` outside ±10000 bps a year
    /// - `SetStressFloor` before parameters are set, at a level that does not exist, or for an
    ///   unknown market
    /// - `SetReduceOnly` for an unknown market, or for every market before `V8`
    /// - `SetClientAuth` once client auth is recorded
    /// - `SetOperator` once the operator is recorded
    /// - `ActivateProtocol` for a version no newer than the one in force, for `V3` or later before
    ///   client auth is recorded, or for `V8` or later before the operator is recorded or while the
    ///   parameters in force fail `ProtocolParams::check_closing_spread`, or for `V11` or later while
    ///   they fail `ProtocolParams::check_notional_weighting`
    /// - `SetToxicityNotional` before parameters are set, over parameters that already weigh markouts
    ///   by notional, or with weighting that fails `NotionalWeighting::validate`
    pub(crate) fn admin(&mut self, cmd: &AdminCommand, events: &mut Vec<Event>) -> Result<(), String> {
        match cmd {
            AdminCommand::SetParams(params) => {
                params.validate()?;
                if self.protocol_version >= ProtocolVersion::V8 {
                    params.check_closing_spread()?;
                }
                if self.protocol_version >= ProtocolVersion::V11 {
                    params.check_notional_weighting()?;
                }
                self.next_rollover = Some(params.rollover.next_after(self.now));
                self.params = Some(params.clone());
                self.restress_all(events);
                if self.protocol_version >= ProtocolVersion::V12 {
                    self.advance_markets(None, events);
                }
                if self.protocol_version >= ProtocolVersion::V6 {
                    self.check_all_closeouts(events);
                }
            }
            AdminCommand::UpsertMarket(spec) => {
                spec.validate()?;
                if spec.schedule.has_exceptions() && self.protocol_version < ProtocolVersion::V4 {
                    return Err(format!("dated closures and special sessions need protocol {:?}", ProtocolVersion::V4));
                }
                self.check_quote_currency(spec)?;
                let currency_changed =
                    self.markets.get(&spec.id).is_some_and(|market| market.spec.quote_currency != spec.quote_currency);
                match self.markets.get_mut(&spec.id) {
                    Some(market) => {
                        // An FX mark belongs to the currency it was taken for.
                        if market.spec.quote_currency != spec.quote_currency {
                            market.fx_mark = None;
                        }
                        market.spec = spec.clone();
                    }
                    None => {
                        self.markets.insert(spec.id, MarketState::new(spec.clone()));
                    }
                }
                self.refresh_session(spec.id, events);
                // A new listing for a market others convert at can change what its index converts
                // at, so it is their price update too, before any close-out runs.
                let converts_here = |market: &MarketState| {
                    market.spec.id != spec.id && market.spec.quote_currency.fx_market() == Some(spec.id)
                };
                let reprices = self.protocol_version >= ProtocolVersion::V7
                    && self.params.is_some()
                    && self.markets.values().any(converts_here);
                if self.protocol_version >= ProtocolVersion::V12 {
                    // A new quote currency drops the FX mark, which a market that converts at its own
                    // index takes back from its own index only when it is processed.
                    self.advance_markets((reprices || currency_changed).then_some(spec.id), events);
                } else if reprices {
                    self.reprice(spec.id, events);
                }
                if self.protocol_version >= ProtocolVersion::V6 && self.params.is_some() {
                    self.check_closeouts(spec.id, events);
                }
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
            AdminCommand::SetReduceOnly { market: Some(id), reduce_only } => {
                self.markets.get_mut(id).ok_or("unknown market")?.reduce_only = *reduce_only;
            }
            AdminCommand::SetReduceOnly { market: None, reduce_only } => {
                if self.protocol_version < ProtocolVersion::V8 {
                    return Err("reduce-only for every market needs protocol V8".into());
                }
                self.global_reduce_only = *reduce_only;
            }
            AdminCommand::SetClientAuth(auth) => {
                if self.client_auth.is_some() {
                    return Err("client auth is already recorded".into());
                }
                self.client_auth = Some(auth.clone());
            }
            AdminCommand::SetToxicityNotional(weighting) => {
                let params = self.params.as_mut().ok_or("protocol params not set")?;
                if params.toxicity.notional.is_some() {
                    return Err("the parameters in force already weigh markouts by notional".into());
                }
                weighting.validate()?;
                params.toxicity.notional = Some(*weighting);
            }
            AdminCommand::SetOperator(operator) => {
                if self.operator.is_some() {
                    return Err("the operator is already recorded".into());
                }
                self.operator = Some(*operator);
            }
            AdminCommand::ActivateProtocol { version } => {
                if *version <= self.protocol_version {
                    return Err(format!("protocol {version:?} is not newer than {:?}", self.protocol_version));
                }
                if *version >= ProtocolVersion::V3 && self.client_auth.is_none() {
                    return Err(format!("protocol {version:?} needs client auth recorded first"));
                }
                if *version >= ProtocolVersion::V8 && self.operator.is_none() {
                    return Err(format!("protocol {version:?} needs the operator recorded first"));
                }
                if *version >= ProtocolVersion::V8
                    && let Some(params) = &self.params
                {
                    params.check_closing_spread()?;
                }
                if *version >= ProtocolVersion::V11
                    && let Some(params) = &self.params
                {
                    params.check_notional_weighting()?;
                }
                // Markouts queued before `V11` carry no notional, so they would move no score, and they
                // are in fill order rather than the due order `V11` keeps.
                if self.protocol_version < ProtocolVersion::V11 && *version >= ProtocolVersion::V11 {
                    while self.pop_markout().is_some() {}
                }
                self.protocol_version = *version;
            }
        }
        Ok(())
    }

    /// Applies an admin command the node sequences unsigned: client auth, the operator, toxicity
    /// notional weighting, and protocol activation at any time, and genesis, which credits its house
    /// capital last.
    /// Error cases:
    /// - from `V8`, an operator command once any deposit or house capital is credited
    /// - any error case of `admin`
    pub(crate) fn unsigned_admin(&mut self, cmd: &AdminCommand, events: &mut Vec<Event>) -> Result<(), String> {
        if self.protocol_version >= ProtocolVersion::V8 && cmd.is_operator_command() && !self.credited_refs.is_empty() {
            return Err("from protocol V8 the operator signs this command once funds are credited".into());
        }
        self.admin(cmd, events)
    }

    /// Applies an admin command the operator signed. A command whose signature and nonce pass uses
    /// the nonce even if it is then refused, so it cannot be replayed from the log.
    /// Error cases:
    /// - before `V8`; the nonce is not used
    /// - a signature that is not the recorded operator's over this command, a nonce other than the
    ///   operator's next, or an expiry passed or too far ahead (see `Engine::authorise_admin`)
    /// - a command only the node sequences: `SetClientAuth`, `SetOperator`, `SetToxicityNotional`, or
    ///   `ActivateProtocol`
    /// - any error case of `admin`
    pub(crate) fn signed_admin(&mut self, req: &AdminRequest, events: &mut Vec<Event>) -> Result<(), String> {
        if self.protocol_version < ProtocolVersion::V8 {
            return Err("signed admin commands need protocol V8".into());
        }
        self.authorise_admin(&req.command, &req.auth).map_err(|reject| auth_failure(&reject))?;
        if !req.command.is_operator_command() {
            return Err("the operator may not send this command".into());
        }
        self.admin(&req.command, events)
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
        let cap = self.market_cap(&self.markets[&market_id].spec, self.house_nav());
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
        let exposure = market.usd_at_mark(market.client_qty.abs().mul(mid), Rounding::Up);
        let util_bps = if cap.is_positive() { exposure.bps_of(cap) } else { i64::MAX };
        let target = auto_stress_target(&params.stress_levels, divergence_bps, move_bps, util_bps);

        let stress = &mut market.stress;
        if target > stress.auto || (target < stress.auto && now - stress.auto_since >= params.stress_cooldown_ms) {
            stress.auto = target;
            stress.auto_since = now;
        }
        let max_level = params.stress_levels.len() - 1;
        set_stress_level(market_id, market, global_floor, max_level, events);
    }

    /// Checks a listing's quote currency against the protocol version and the markets already listed.
    /// Before `V7` every market is quoted in USD, whatever its symbol.
    /// Error cases:
    /// - a quote currency other than USD before `V7`
    /// - from `V7`, a listing that fails `MarketSpec::check_fx_market`, or that makes a market which
    ///   converts at it fail it
    /// - a change of quote currency while an account holds a position in the market
    fn check_quote_currency(&self, spec: &MarketSpec) -> Result<(), String> {
        if self.protocol_version < ProtocolVersion::V7 {
            if spec.quote_currency.is_usd() {
                return Ok(());
            }
            return Err(format!("a quote currency other than USD needs protocol {:?}", ProtocolVersion::V7));
        }
        let symbol = |id: MarketId| {
            if id == spec.id {
                Some(spec.symbol.as_str())
            } else {
                self.markets.get(&id).map(|market| market.spec.symbol.as_str())
            }
        };
        spec.check_fx_market(symbol)?;
        // A market others convert at must stay their pair.
        let dependents = self.markets.values().map(|market| &market.spec);
        for dependent in dependents.filter(|d| d.id != spec.id && d.quote_currency.fx_market() == Some(spec.id)) {
            dependent.check_fx_market(symbol).map_err(|e| format!("market {} converts here: {e}", dependent.id))?;
        }
        let changed =
            self.markets.get(&spec.id).is_some_and(|market| market.spec.quote_currency != spec.quote_currency);
        if changed && self.accounts.values().any(|acct| acct.positions.contains_key(&spec.id)) {
            return Err(format!("market {} has open positions, so its quote currency cannot change", spec.id));
        }
        Ok(())
    }

    /// Recomputes every market's effective stress level after a floor or parameter change.
    fn restress_all(&mut self, events: &mut Vec<Event>) {
        let Some(params) = &self.params else { return };
        let max_level = params.stress_levels.len() - 1;
        let global_floor = self.global_stress_floor;
        for (market_id, market) in self.markets.iter_mut() {
            set_stress_level(*market_id, market, global_floor, max_level, events);
        }
    }
}

/// The reason an admin command fails its signature, nonce, or expiry check.
fn auth_failure(reject: &Reject) -> String {
    match reject {
        Reject::InvalidSignature => "invalid signature".into(),
        Reject::InvalidNonce { expected } => format!("invalid nonce: expected {expected}"),
        Reject::Expired { expires_at } => format!("expired at {expires_at}"),
        Reject::ExpiryTooFar { latest } => format!("expiry after the latest allowed, {latest}"),
        // `authorise_admin` refuses only for the reasons above.
        other => serde_json::to_string(other).expect("rejections serialise"),
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
