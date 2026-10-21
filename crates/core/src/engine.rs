//! The engine state machine: `apply(sequenced command) -> events`.
//!
//! Single-threaded and deterministic. Time comes only from the sequencer's stamp. This file holds
//! the state, dispatch, time, prices, and USD valuation; funds and financing live in `funds.rs`,
//! admin and stress in `admin.rs`, client request auth in `auth.rs`, orders and pricing in
//! `execution.rs`, and close-outs in `closeout.rs`.

use std::collections::{BTreeMap, VecDeque};
use std::ops::Bound;

use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::account::{Account, Position};
use crate::auth::{ClientAuth, Operator};
use crate::command::{Command, Sequenced};
use crate::event::{Event, Reject};
use crate::fixed::Rounding;
use crate::market::{MarketSpec, QuoteCurrency};
use crate::merkle::state::{Changed, StateTree, is_zero};
use crate::params::ProtocolParams;
use crate::price::{Index, MAX_QUOTE_LEAD_MS, SourceQuote, compute_index};
use crate::protocol::ProtocolVersion;
use crate::tracked::{TrackedMap, TrackedSet};
use crate::types::{AccountId, MarketId, Millis, OrderId, Side};

/// Clamp on a single markout, so one wild print cannot dominate an account's toxicity score.
const MAX_MARKOUT_BPS: i64 = 1_000;

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum SequenceError {
    #[error("expected seq {expected}, got {got}")]
    Gap { expected: u64, got: u64 },
    #[error("time went backwards: {last} -> {got}")]
    TimeReversed { last: Millis, got: Millis },
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Engine {
    pub(crate) params: Option<ProtocolParams>,
    pub(crate) markets: TrackedMap<MarketId, MarketState>,
    pub(crate) accounts: TrackedMap<AccountId, Account>,
    /// House cash. NAV is this minus clients' unrealised PnL.
    pub(crate) house_cash: Fixed,
    /// Annual benchmark rates in bps, keyed by name ("SOFR", ...).
    pub(crate) benchmarks: BTreeMap<String, i64>,
    /// Custody references already credited (deposits and house capital), so none applies twice.
    pub(crate) credited_refs: TrackedSet<String>,
    pub(crate) global_stress_floor: usize,
    pub(crate) next_order_id: OrderId,
    pub(crate) next_rollover: Option<Millis>,
    pub(crate) markouts: VecDeque<Markout>,
    pub(crate) last_seq: Option<u64>,
    pub(crate) now: Millis,
    // The fields below are left out of the encoding while they hold their defaults, so a log that
    // never activates a protocol version keeps the roots it was written with.
    /// The rules in force.
    #[serde(default, skip_serializing_if = "ProtocolVersion::is_v2")]
    pub(crate) protocol_version: ProtocolVersion,
    /// How client requests are authorised from `V3`; recorded once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) client_auth: Option<ClientAuth>,
    /// Each signer's next request nonce, the accounts' and the operator's; a signer missing here
    /// is at 0.
    #[serde(default, skip_serializing_if = "TrackedMap::is_empty")]
    pub(crate) request_nonces: TrackedMap<AccountId, u64>,
    /// Who signs admin commands from `V8`; recorded once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) operator: Option<Operator>,
    /// Every market is reduce-only, whatever its own flag says.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) global_reduce_only: bool,
    /// The state tree's index for the front markout: 0 before `V9`, then one more per markout.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(crate) markout_base: u64,
    // Not state and not encoded; an engine rebuilt without them hashes every leaf at its first root.
    /// Markouts, positions, and resting orders changed since the last state root.
    #[serde(skip)]
    pub(crate) changed: Changed,
    /// The state tree as of the last root under `V9`; absent until then.
    #[serde(skip)]
    pub(crate) tree: Option<StateTree>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MarketState {
    pub spec: MarketSpec,
    pub quotes: BTreeMap<String, SourceQuote>,
    pub open: bool,
    pub reduce_only: bool,
    /// Last index mid marked while the market and its FX market had a price. Positions are marked
    /// here, including while the market is closed or its FX market has no price.
    pub mark: Option<Fixed>,
    /// Last index of the FX market that could convert this market's quote currency: what its
    /// positions are valued at in USD. `None` for a USD market, and left out of the encoding then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fx_mark: Option<Fixed>,
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

/// A fill waiting to be marked against the index once its horizon has passed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Markout {
    pub due: Millis,
    pub account: AccountId,
    pub market: MarketId,
    pub side: Side,
    pub price: Fixed,
}

impl MarketState {
    /// A newly listed market: closed until its first session check, with no price or positions.
    pub(crate) fn new(spec: MarketSpec) -> Self {
        MarketState {
            spec,
            quotes: BTreeMap::new(),
            open: false,
            reduce_only: false,
            mark: None,
            fx_mark: None,
            client_qty: Fixed::ZERO,
            client_cost: Fixed::ZERO,
            stress: StressState::default(),
            resting: BTreeMap::new(),
        }
    }

    /// `amount` in this market's quote currency, in USD at its FX mark, rounded as `rounding` says.
    pub fn usd_at_mark(&self, amount: Fixed, rounding: Rounding) -> Fixed {
        self.spec.quote_currency.to_usd(amount, self.fx_mark, rounding)
    }
}

impl Engine {
    /// An empty engine, before genesis.
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies one sequenced command and returns the events it produced. A refused command is an
    /// event, not an error; an error means the sequencer broke its own contract.
    /// Error cases:
    /// - `seq` is not exactly one more than the last applied (or 0 for the first)
    /// - `ts` is earlier than the last applied
    pub fn apply(&mut self, sequenced: &Sequenced) -> Result<Vec<Event>, SequenceError> {
        let expected = self.last_seq.map_or(0, |n| n + 1);
        if sequenced.seq != expected {
            return Err(SequenceError::Gap { expected, got: sequenced.seq });
        }
        if sequenced.ts < self.now {
            return Err(SequenceError::TimeReversed { last: self.now, got: sequenced.ts });
        }
        self.last_seq = Some(sequenced.seq);
        self.now = sequenced.ts;

        let mut events = Vec::new();
        self.run_rollovers(&mut events);
        match &sequenced.cmd {
            Command::Prices { market, quotes } => self.on_prices(*market, quotes, &mut events),
            Command::Tick => self.refresh_all_sessions(&mut events),
            Command::Deposit { account, amount, chain_ref } => self.deposit(*account, *amount, chain_ref, &mut events),
            Command::Withdraw { account, amount, auth } => self.withdraw(*account, *amount, auth.as_ref(), &mut events),
            Command::HouseCapital { amount, chain_ref } => self.house_capital(*amount, chain_ref, &mut events),
            Command::PlaceOrder(req) => self.place_order(req, &mut events),
            Command::CancelOrder { account, order_id, auth } => {
                self.cancel_order(*account, *order_id, auth.as_ref(), &mut events)
            }
            Command::Admin(cmd) => match self.unsigned_admin(cmd, &mut events) {
                Ok(()) => events.push(Event::AdminApplied),
                Err(reason) => events.push(Event::AdminRejected { reason }),
            },
            Command::SignedAdmin(req) => match self.signed_admin(req, &mut events) {
                Ok(()) => events.push(Event::AdminApplied),
                Err(reason) => events.push(Event::AdminRejected { reason }),
            },
        }
        // After the command, so a price arriving at the horizon is the one marked against.
        self.process_markouts();
        Ok(events)
    }

    /// Seq of the last applied command, or `None` before genesis.
    pub fn last_seq(&self) -> Option<u64> {
        self.last_seq
    }

    /// Sequenced time of the last applied command.
    pub fn now(&self) -> Millis {
        self.now
    }

    /// The protocol parameters, once set.
    pub fn params(&self) -> Option<&ProtocolParams> {
        self.params.as_ref()
    }

    /// The protocol version in force.
    pub fn protocol_version(&self) -> ProtocolVersion {
        self.protocol_version
    }

    /// How client requests are authorised from `V3`, once recorded.
    pub fn client_auth(&self) -> Option<&ClientAuth> {
        self.client_auth.as_ref()
    }

    /// The nonce `account`'s next signed request must carry; for the operator's address, its next
    /// admin command's.
    pub fn next_request_nonce(&self, account: AccountId) -> u64 {
        self.request_nonces.get(&account).copied().unwrap_or(0)
    }

    /// Who signs admin commands from `V8`, once recorded.
    pub fn operator(&self) -> Option<&Operator> {
        self.operator.as_ref()
    }

    /// Whether a market takes no new risk: its own flag, or every market's.
    pub fn is_reduce_only(&self, market: &MarketState) -> bool {
        market.reduce_only || self.global_reduce_only
    }

    /// Every listed market, by id.
    pub fn markets(&self) -> &BTreeMap<MarketId, MarketState> {
        &self.markets
    }

    /// Every account that has ever been credited, by id.
    pub fn accounts(&self) -> &BTreeMap<AccountId, Account> {
        &self.accounts
    }

    /// Whether the custody reference `chain_ref` has been credited.
    pub fn is_credited(&self, chain_ref: &str) -> bool {
        self.credited_refs.contains(chain_ref)
    }

    /// The credited custody references that start with `prefix`, in order.
    pub fn credited_refs_with_prefix<'a>(&'a self, prefix: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        let from = (Bound::Included(prefix), Bound::Unbounded);
        self.credited_refs.range::<str, _>(from).map(String::as_str).take_while(move |r| r.starts_with(prefix))
    }

    /// House cash, before clients' unrealised PnL.
    pub fn house_cash(&self) -> Fixed {
        self.house_cash
    }

    /// House cash minus clients' unrealised PnL at current marks, in USD. Clients' PnL rounds up, so
    /// NAV, and every cap set from it, rounds down.
    pub fn house_nav(&self) -> Fixed {
        let client_upnl: Fixed = self
            .markets
            .values()
            .filter_map(|market| {
                let upnl = |mark: Fixed| mark.mul(market.client_qty) - market.client_cost;
                market.mark.map(|mark| market.usd_at_mark(upnl(mark), Rounding::Up))
            })
            .sum();
        self.house_cash - client_upnl
    }

    /// Sum over markets of the house's absolute net notional, in USD, each rounded up.
    pub fn house_gross_exposure(&self) -> Fixed {
        self.markets
            .values()
            .filter_map(|market| {
                market.mark.map(|mark| market.usd_at_mark(market.client_qty.abs().mul(mark), Rounding::Up))
            })
            .sum()
    }

    /// The cap on the house's absolute net notional in `spec`'s market at house NAV `nav`: its share
    /// of NAV, or from `V6` the larger of that and the market's floor.
    pub(crate) fn market_cap(&self, spec: &MarketSpec, nav: Fixed) -> Fixed {
        self.floored(nav.mul_bps(spec.house_cap_bps), spec.house_cap_floor)
    }

    /// The cap on the house's gross exposure across markets at house NAV `nav`: its share of NAV,
    /// or from `V6` the larger of that and the total floor.
    pub(crate) fn total_cap(&self, params: &ProtocolParams, nav: Fixed) -> Fixed {
        self.floored(nav.mul_bps(params.house_total_cap_bps), params.house_total_cap_floor)
    }

    /// (equity, margin requirement) in USD at current marks, optionally overriding one market's mark
    /// and FX index.
    pub fn equity_and_margin(
        &self,
        acct: &Account,
        mark_override: Option<(MarketId, Fixed, Option<Fixed>)>,
    ) -> (Fixed, Fixed) {
        self.equity_and_margin_at(acct, |market_id, market, pos| match mark_override {
            Some((id, mid, fx)) if id == market_id => (mid, fx),
            _ => (market.mark.unwrap_or(pos.entry), market.fx_mark),
        })
    }

    /// (equity, margin requirement) in USD with each position valued at the price and FX index
    /// `price_of(market id, market, position)` gives: PnL converts rounded down and notional rounded
    /// up. From `V10` the requirement rounds up, from each position's notional to each band's charge.
    pub(crate) fn equity_and_margin_at(
        &self,
        acct: &Account,
        price_of: impl Fn(MarketId, &MarketState, &Position) -> (Fixed, Option<Fixed>),
    ) -> (Fixed, Fixed) {
        acct.positions
            .iter()
            .filter_map(|(market_id, pos)| self.markets.get(market_id).map(|market| (*market_id, pos, market)))
            .fold((acct.balance, Fixed::ZERO), |(equity, margin), (market_id, pos, market)| {
                let (price, fx) = price_of(market_id, market, pos);
                let usd = |amount, rounding| market.spec.quote_currency.to_usd(amount, fx, rounding);
                let notional = usd(pos.qty.abs().mul_rounded(price, self.charge_rounding()), Rounding::Up);
                let pnl = usd(pos.unrealised(price, self.protocol_version), Rounding::Down);
                (equity + pnl, margin + market.spec.margin_for(notional, self.charge_rounding()))
            })
    }

    /// The FX index a fill in `market_id` converts at now: its FX market's live index, or `None` for
    /// a USD market.
    /// Error cases:
    /// - the FX market's schedule is closed at the sequenced time, or it has no valid index, or one
    ///   that cannot convert the currency
    /// - the market has no FX mark yet, so a position in it could not be valued
    pub(crate) fn live_fx(&self, market_id: MarketId) -> Result<Option<Fixed>, Reject> {
        let market = &self.markets[&market_id];
        let currency = market.spec.quote_currency;
        let Some(fx_market) = currency.fx_market() else { return Ok(None) };
        // Listing checks the FX market is listed, and no market is ever removed.
        let fx = self.markets.get(&fx_market).expect("an FX market is listed before markets that use it");
        let no_price = Reject::NoFxPrice { fx_market };
        // The schedule, not the stored flag, which only the FX market's own commands refresh.
        if !fx.spec.schedule.is_open(self.now) || market.fx_mark.is_none() {
            return Err(no_price);
        }
        match compute_index(&fx.quotes, &fx.spec.price_rules, self.now) {
            Ok(index) if currency.converts_at(index.mid) => Ok(Some(index.mid)),
            _ => Err(no_price),
        }
    }

    /// Updates a market's open flag from its schedule, emitting the transition.
    pub(crate) fn refresh_session(&mut self, market_id: MarketId, events: &mut Vec<Event>) {
        let now = self.now;
        let Some(market) = self.markets.get(&market_id) else { return };
        let open = market.spec.schedule.is_open(now);
        if open == market.open {
            return;
        }
        let market = self.markets.get_mut(&market_id).expect("found above");
        market.open = open;
        if open {
            market.stress.move_ref = None;
            events.push(Event::MarketOpened { market: market_id });
        } else {
            events.push(Event::MarketClosed { market: market_id });
        }
    }

    /// `share`, or from `V6` the larger of `share` and `floor`.
    fn floored(&self, share: Fixed, floor: Fixed) -> Fixed {
        if self.protocol_version >= ProtocolVersion::V6 { share.max(floor) } else { share }
    }

    /// Refreshes every market's session, so opens and closes fire even without price traffic.
    fn refresh_all_sessions(&mut self, events: &mut Vec<Event>) {
        let ids: Vec<MarketId> = self.markets.keys().copied().collect();
        for market_id in ids {
            self.refresh_session(market_id, events);
        }
    }

    /// Stores venue quotes, then marks the market from them (`reprice`). Malformed quotes, and quotes
    /// stamped more than `MAX_QUOTE_LEAD_MS` after the sequenced time, are refused one by one.
    fn on_prices(&mut self, market_id: MarketId, quotes: &[SourceQuote], events: &mut Vec<Event>) {
        let now = self.now;
        let Some(market) = self.markets.get_mut(&market_id) else {
            events.push(Event::PricesRejected { market: market_id, reason: "unknown market".into() });
            return;
        };
        for quote in quotes {
            if !quote.is_well_formed() {
                let reason = format!("malformed quote from {}", quote.source);
                events.push(Event::PricesRejected { market: market_id, reason });
                continue;
            }
            let lead_ms = quote.ts.saturating_sub(now);
            if lead_ms > MAX_QUOTE_LEAD_MS {
                let reason = format!("quote from {} stamped {lead_ms}ms ahead of the sequenced time", quote.source);
                events.push(Event::PricesRejected { market: market_id, reason });
                continue;
            }
            // A stored quote stamped after the sequenced time does not count yet, so it never holds
            // back a replacement.
            if market.quotes.get(&quote.source).is_none_or(|old| quote.ts >= old.ts || old.ts > now) {
                market.quotes.insert(quote.source.clone(), quote.clone());
            }
        }
        self.reprice(market_id, events);
    }

    /// Marks a market from its stored quotes, as a price update does: if it is open with a valid
    /// index, gives that index to the markets that convert at it as their FX mark, and marks the
    /// market and each of those that is open with a price from its own quotes. Only once every mark
    /// is set does each marked market update its stress and trigger its resting orders, and
    /// close-outs run in every market whose value moved.
    pub(crate) fn reprice(&mut self, market_id: MarketId, events: &mut Vec<Event>) {
        let now = self.now;
        self.refresh_session(market_id, events);
        let market = &self.markets[&market_id];
        if !market.open || self.params.is_none() {
            return;
        }
        let Ok(index) = compute_index(&market.quotes, &market.spec.price_rules, now) else { return };
        let dependents: Vec<MarketId> =
            self.mark_fx(market_id, index.mid).into_iter().filter(|id| *id != market_id).collect();
        let mut marked = Vec::new();
        if self.set_mark(market_id, index) {
            marked.push((market_id, index));
        }
        for &dependent in &dependents {
            self.refresh_session(dependent, events);
            let market = &self.markets[&dependent];
            let live =
                if market.open { compute_index(&market.quotes, &market.spec.price_rules, now).ok() } else { None };
            if let Some(index) = live
                && self.set_mark(dependent, index)
            {
                marked.push((dependent, index));
            }
        }
        for (id, index) in &marked {
            self.update_stress(*id, index.mid, index.divergence_bps, events);
        }
        for (id, _) in &marked {
            self.trigger_resting(*id, events);
        }
        if marked.first().is_some_and(|(id, _)| *id == market_id) {
            self.check_closeouts(market_id, events);
        }
        // A new FX mark moves the USD value of every market that took it, marked or not.
        for dependent in dependents {
            self.check_closeouts(dependent, events);
        }
    }

    /// Marks a market at `index`, its live index, and returns true, unless its FX market has no
    /// price: then it has none either, keeps its last mark, and this returns false.
    fn set_mark(&mut self, market_id: MarketId, index: Index) -> bool {
        if self.live_fx(market_id).is_err() {
            return false;
        }
        self.markets.get_mut(&market_id).expect("market exists").mark = Some(index.mid);
        true
    }

    /// Sets `mid` as the FX mark of every market that converts at `fx_market`'s index and that `mid`
    /// can convert, and returns those markets.
    fn mark_fx(&mut self, fx_market: MarketId, mid: Fixed) -> Vec<MarketId> {
        let converts = |currency: QuoteCurrency| currency.fx_market() == Some(fx_market) && currency.converts_at(mid);
        let marked: Vec<MarketId> =
            self.markets.iter().filter(|(_, m)| converts(m.spec.quote_currency)).map(|(id, _)| *id).collect();
        // Only the markets it marks are taken for change, so the state root rehashes those alone.
        marked.iter().for_each(|id| self.markets.get_mut(id).expect("found above").fx_mark = Some(mid));
        marked
    }

    /// Marks every fill whose horizon has passed and folds the markout into its account's score.
    fn process_markouts(&mut self) {
        let Some(params) = &self.params else { return };
        let weight = params.toxicity.ewma_weight_bps;
        while self.markouts.front().is_some_and(|markout| markout.due <= self.now) {
            let markout = self.pop_markout().expect("front exists");
            let mark = self.markets.get(&markout.market).and_then(|market| market.mark);
            let (Some(mark), Some(acct)) = (mark, self.accounts.get_mut(&markout.account)) else {
                continue;
            };
            let bps = (mark - markout.price)
                .mul_int(markout.side.sign())
                .bps_of(markout.price)
                .clamp(-MAX_MARKOUT_BPS, MAX_MARKOUT_BPS);
            acct.toxicity_bps += (bps - acct.toxicity_bps) * weight / 10_000;
        }
    }
}
