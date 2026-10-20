//! The engine state machine: `apply(sequenced command) -> events`.
//!
//! Single-threaded and deterministic. Time comes only from the sequencer's
//! stamp. Order handling, pricing and risk live in `execution.rs`; this file
//! covers dispatch, time, funds, admin and the state root.

use std::collections::{BTreeMap, VecDeque};

use serde::{Deserialize, Serialize};
use sha3::{Digest, Keccak256};

use crate::account::Account;
use crate::command::{AdminCommand, Command, Sequenced};
use crate::event::{Event, Reject};
use crate::market::MarketSpec;
use crate::params::ProtocolParams;
use crate::price::{compute_index, SourceQuote};
use crate::types::{AccountId, MarketId, Millis, OrderId, Side};
use crate::Fixed;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Engine {
    pub(crate) params: Option<ProtocolParams>,
    pub(crate) markets: BTreeMap<MarketId, MarketState>,
    pub(crate) accounts: BTreeMap<AccountId, Account>,
    /// House cash. NAV is this minus clients' unrealised PnL.
    pub(crate) house_cash: Fixed,
    /// Annual benchmark rates in bps, keyed by name ("SOFR", ...).
    pub(crate) benchmarks: BTreeMap<String, i64>,
    pub(crate) global_stress_floor: usize,
    pub(crate) next_order_id: OrderId,
    pub(crate) next_rollover: Option<Millis>,
    pub(crate) markouts: VecDeque<Markout>,
    pub(crate) last_seq: Option<u64>,
    pub(crate) now: Millis,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MarketState {
    pub spec: MarketSpec,
    pub quotes: BTreeMap<String, SourceQuote>,
    pub open: bool,
    pub reduce_only: bool,
    /// Last valid index mid. Positions are marked here, including while the
    /// market is closed.
    pub mark: Option<Fixed>,
    /// Sum of client position qty; the house holds the opposite.
    pub client_qty: Fixed,
    /// Sum of client `entry * qty`, so unrealised PnL is O(1) per market.
    pub client_cost: Fixed,
    pub stress: StressState,
    /// Resting orders in time priority.
    pub resting: BTreeMap<OrderId, AccountId>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StressState {
    /// Effective level: max(auto, floor, global floor).
    pub level: usize,
    pub auto: usize,
    pub auto_since: Millis,
    pub floor: usize,
    pub move_ref: Option<(Millis, Fixed)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Markout {
    pub due: Millis,
    pub account: AccountId,
    pub market: MarketId,
    pub side: Side,
    pub price: Fixed,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum SequenceError {
    #[error("expected seq {expected}, got {got}")]
    Gap { expected: u64, got: u64 },
    #[error("time went backwards: {last} -> {got}")]
    TimeReversed { last: Millis, got: Millis },
}

impl Engine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply(&mut self, s: &Sequenced) -> Result<Vec<Event>, SequenceError> {
        let expected = self.last_seq.map_or(0, |n| n + 1);
        if s.seq != expected {
            return Err(SequenceError::Gap { expected, got: s.seq });
        }
        if s.ts < self.now {
            return Err(SequenceError::TimeReversed { last: self.now, got: s.ts });
        }
        self.last_seq = Some(s.seq);
        self.now = s.ts;

        let mut ev = Vec::new();
        self.run_rollovers(&mut ev);
        match &s.cmd {
            Command::Prices { market, quotes } => self.on_prices(*market, quotes, &mut ev),
            Command::Tick => {
                let ids: Vec<MarketId> = self.markets.keys().copied().collect();
                for m in ids {
                    self.refresh_session(m, &mut ev);
                }
            }
            Command::Deposit { account, amount, .. } => self.deposit(*account, *amount, &mut ev),
            Command::Withdraw { account, amount } => self.withdraw(*account, *amount, &mut ev),
            Command::HouseCapital { amount, .. } => self.house_capital(*amount, &mut ev),
            Command::PlaceOrder(req) => self.place_order(req, &mut ev),
            Command::CancelOrder { account, order_id } => self.cancel_order(*account, *order_id, &mut ev),
            Command::Admin(cmd) => match self.admin(cmd, &mut ev) {
                Ok(()) => ev.push(Event::AdminApplied),
                Err(reason) => ev.push(Event::AdminRejected { reason }),
            },
        }
        // After the command, so a price arriving at the horizon is the one marked against.
        self.process_markouts();
        Ok(ev)
    }

    /// Keccak-256 of the canonical state encoding. Posted on-chain at
    /// checkpoints; anyone replaying the log must get the same root.
    pub fn state_root(&self) -> [u8; 32] {
        let bytes = serde_json::to_vec(self).expect("engine state serialises");
        Keccak256::digest(bytes).into()
    }

    pub fn last_seq(&self) -> Option<u64> {
        self.last_seq
    }

    pub fn now(&self) -> Millis {
        self.now
    }

    pub fn params(&self) -> Option<&ProtocolParams> {
        self.params.as_ref()
    }

    pub fn markets(&self) -> &BTreeMap<MarketId, MarketState> {
        &self.markets
    }

    pub fn accounts(&self) -> &BTreeMap<AccountId, Account> {
        &self.accounts
    }

    pub fn house_cash(&self) -> Fixed {
        self.house_cash
    }

    /// House cash minus clients' unrealised PnL at current marks.
    pub fn house_nav(&self) -> Fixed {
        let client_upnl: Fixed = self
            .markets
            .values()
            .filter_map(|ms| ms.mark.map(|mark| mark.mul(ms.client_qty) - ms.client_cost))
            .sum();
        self.house_cash - client_upnl
    }

    /// Sum over markets of the house's absolute net notional.
    pub fn house_gross_exposure(&self) -> Fixed {
        self.markets
            .values()
            .filter_map(|ms| ms.mark.map(|mark| ms.client_qty.abs().mul(mark)))
            .sum()
    }

    /// (equity, margin requirement) at current marks, optionally overriding
    /// one market's mark.
    pub fn equity_and_margin(&self, acct: &Account, mark_override: Option<(MarketId, Fixed)>) -> (Fixed, Fixed) {
        let mut equity = acct.balance;
        let mut margin = Fixed::ZERO;
        for (m, pos) in &acct.positions {
            let Some(ms) = self.markets.get(m) else { continue };
            let mark = match mark_override {
                Some((om, mid)) if om == *m => mid,
                _ => ms.mark.unwrap_or(pos.entry),
            };
            equity += pos.unrealised(mark);
            margin += ms.spec.margin_for(pos.qty.abs().mul(mark));
        }
        (equity, margin)
    }

    pub(crate) fn refresh_session(&mut self, m: MarketId, ev: &mut Vec<Event>) {
        let now = self.now;
        let Some(ms) = self.markets.get_mut(&m) else { return };
        let open = ms.spec.schedule.is_open(now);
        if open == ms.open {
            return;
        }
        ms.open = open;
        if open {
            ms.stress.move_ref = None;
            ev.push(Event::MarketOpened { market: m });
        } else {
            ev.push(Event::MarketClosed { market: m });
        }
    }

    fn on_prices(&mut self, m: MarketId, quotes: &[SourceQuote], ev: &mut Vec<Event>) {
        let Some(ms) = self.markets.get_mut(&m) else { return };
        for q in quotes {
            let newer = ms.quotes.get(&q.source).is_none_or(|old| q.ts >= old.ts);
            if newer {
                ms.quotes.insert(q.source.clone(), q.clone());
            }
        }
        self.refresh_session(m, ev);
        let ms = &self.markets[&m];
        if !ms.open || self.params.is_none() {
            return;
        }
        let Ok(index) = compute_index(&ms.quotes, &ms.spec.price_rules, self.now) else { return };
        self.markets.get_mut(&m).expect("market exists").mark = Some(index.mid);
        self.update_stress(m, index.mid, index.divergence_bps, ev);
        self.trigger_resting(m, ev);
        self.check_closeouts(m, ev);
    }

    fn update_stress(&mut self, m: MarketId, mid: Fixed, divergence_bps: i64, ev: &mut Vec<Event>) {
        let nav = self.house_nav();
        let now = self.now;
        let global_floor = self.global_stress_floor;
        let params = self.params.as_ref().expect("checked by caller");
        let ms = self.markets.get_mut(&m).expect("market exists");

        let move_bps = match ms.stress.move_ref {
            Some((t, p)) if now - t < params.stress_move_window_ms => (mid - p).abs().bps_of(p),
            _ => {
                ms.stress.move_ref = Some((now, mid));
                0
            }
        };
        let cap = nav.mul_bps(ms.spec.house_cap_bps);
        let util_bps =
            if cap.is_positive() { ms.client_qty.abs().mul(mid).bps_of(cap) } else { i64::MAX };

        let target = params
            .stress_levels
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
            .map_or(0, |(i, _)| i);

        let s = &mut ms.stress;
        if target > s.auto || (target < s.auto && now - s.auto_since >= params.stress_cooldown_ms) {
            s.auto = target;
            s.auto_since = now;
        }
        let max_level = params.stress_levels.len() - 1;
        set_stress_level(m, ms, global_floor, max_level, ev);
    }

    fn process_markouts(&mut self) {
        let Some(params) = &self.params else { return };
        while self.markouts.front().is_some_and(|mk| mk.due <= self.now) {
            let mk = self.markouts.pop_front().expect("front exists");
            let mark = self.markets.get(&mk.market).and_then(|ms| ms.mark);
            let (Some(mark), Some(acct)) = (mark, self.accounts.get_mut(&mk.account)) else {
                continue;
            };
            let markout = (mark - mk.price).mul_int(mk.side.sign()).bps_of(mk.price).clamp(-1_000, 1_000);
            acct.toxicity_bps += (markout - acct.toxicity_bps) * params.toxicity.ewma_weight_bps / 10_000;
        }
    }

    fn run_rollovers(&mut self, ev: &mut Vec<Event>) {
        let Some(params) = &self.params else { return };
        let rollover = params.rollover.clone();
        while let Some(at) = self.next_rollover.filter(|at| *at <= self.now) {
            self.charge_financing(ev);
            self.next_rollover = Some(rollover.next_after(at));
        }
    }

    /// One day of overnight financing on every open position. Longs pay
    /// benchmark + markup; shorts receive benchmark - markup.
    fn charge_financing(&mut self, ev: &mut Vec<Event>) {
        for (account, acct) in &mut self.accounts {
            for (m, pos) in &acct.positions {
                let Some(ms) = self.markets.get(m) else { continue };
                let Some(mark) = ms.mark else { continue };
                let fin = &ms.spec.financing;
                let benchmark = self.benchmarks.get(&fin.benchmark).copied().unwrap_or(0);
                let annual_bps = if pos.qty.is_positive() {
                    benchmark + fin.markup_bps
                } else {
                    fin.markup_bps - benchmark
                };
                let amount = pos.qty.abs().mul(mark).mul_bps(annual_bps).div_int(fin.day_basis);
                if amount.is_zero() {
                    continue;
                }
                acct.balance -= amount;
                self.house_cash += amount;
                ev.push(Event::Financing { account: *account, market: *m, amount });
            }
        }
    }

    fn deposit(&mut self, account: AccountId, amount: Fixed, ev: &mut Vec<Event>) {
        if !amount.is_positive() {
            return;
        }
        self.accounts.entry(account).or_default().balance += amount;
        ev.push(Event::Deposited { account, amount });
    }

    fn withdraw(&mut self, account: AccountId, amount: Fixed, ev: &mut Vec<Event>) {
        let reject = |available, ev: &mut Vec<Event>| {
            ev.push(Event::WithdrawalRejected { account, amount, reason: Reject::InsufficientFunds { available } })
        };
        let Some(acct) = self.accounts.get(&account) else {
            return reject(Fixed::ZERO, ev);
        };
        let (equity, margin) = self.equity_and_margin(acct, None);
        let available = acct.balance.min(equity - margin).max(Fixed::ZERO);
        if !amount.is_positive() || amount > available {
            return reject(available, ev);
        }
        let acct = self.accounts.get_mut(&account).expect("checked");
        acct.balance -= amount;
        acct.withdrawal_nonce += 1;
        ev.push(Event::WithdrawalApproved { account, amount, nonce: acct.withdrawal_nonce });
    }

    fn house_capital(&mut self, amount: Fixed, ev: &mut Vec<Event>) {
        if amount.is_zero() || (amount.is_negative() && !(self.house_nav() + amount).is_positive()) {
            ev.push(Event::AdminRejected { reason: "house capital move would leave NAV <= 0".into() });
            return;
        }
        self.house_cash += amount;
        ev.push(Event::HouseCapitalMoved { amount, nav: self.house_nav() });
    }

    fn admin(&mut self, cmd: &AdminCommand, ev: &mut Vec<Event>) -> Result<(), String> {
        match cmd {
            AdminCommand::SetParams(params) => {
                params.validate()?;
                self.next_rollover = Some(params.rollover.next_after(self.now));
                self.params = Some(params.clone());
                self.restress_all(ev);
            }
            AdminCommand::UpsertMarket(spec) => {
                spec.validate()?;
                match self.markets.get_mut(&spec.id) {
                    Some(ms) => ms.spec = spec.clone(),
                    None => {
                        self.markets.insert(
                            spec.id,
                            MarketState {
                                spec: spec.clone(),
                                quotes: BTreeMap::new(),
                                open: false,
                                reduce_only: false,
                                mark: None,
                                client_qty: Fixed::ZERO,
                                client_cost: Fixed::ZERO,
                                stress: StressState::default(),
                                resting: BTreeMap::new(),
                            },
                        );
                    }
                }
                self.refresh_session(spec.id, ev);
            }
            AdminCommand::SetBenchmarkRate { benchmark, annual_bps } => {
                self.benchmarks.insert(benchmark.clone(), *annual_bps);
            }
            AdminCommand::SetStressFloor { market, level } => {
                let levels = self.params.as_ref().ok_or("protocol params not set")?.stress_levels.len();
                if *level >= levels {
                    return Err(format!("stress level {level} does not exist"));
                }
                match market {
                    None => self.global_stress_floor = *level,
                    Some(m) => self.markets.get_mut(m).ok_or("unknown market")?.stress.floor = *level,
                }
                self.restress_all(ev);
            }
            AdminCommand::SetReduceOnly { market, reduce_only } => {
                self.markets.get_mut(market).ok_or("unknown market")?.reduce_only = *reduce_only;
            }
        }
        Ok(())
    }

    fn restress_all(&mut self, ev: &mut Vec<Event>) {
        let Some(params) = &self.params else { return };
        let max_level = params.stress_levels.len() - 1;
        let global_floor = self.global_stress_floor;
        for (m, ms) in &mut self.markets {
            set_stress_level(*m, ms, global_floor, max_level, ev);
        }
    }
}

fn set_stress_level(m: MarketId, ms: &mut MarketState, global_floor: usize, max_level: usize, ev: &mut Vec<Event>) {
    let s = &mut ms.stress;
    let level = s.auto.max(s.floor).max(global_floor).min(max_level);
    if level != s.level {
        s.level = level;
        ev.push(Event::StressChanged { market: m, level });
    }
}
