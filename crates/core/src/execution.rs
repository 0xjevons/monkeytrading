//! Orders, pricing, and fills.
//!
//! Every fill is against the house at the live index mid plus a spread made of three parts:
//! - the stress level's minimum spread (zero in normal conditions);
//! - the account's toxicity add-on (zero for normal flow);
//! - the size spread, charged only on the slice of the trade that grows the account's absolute
//!   position through the market's size bands.
//!
//! Normal trades therefore fill at the mid. A market quoted in another currency than USD fills only
//! while its FX market has a price, and its realised PnL converts to USD at that FX index.

use crate::Fixed;
use crate::account::RestingOrder;
use crate::auth::RequestAuth;
use crate::command::{OrderKind, OrderRequest};
use crate::eip712::{cancel_hash, order_hash};
use crate::engine::{Engine, Markout};
use crate::event::{CancelReason, Event, Fill, Reject};
use crate::fixed::Rounding;
use crate::market::staircase;
use crate::price::compute_index;
use crate::protocol::ProtocolVersion;
use crate::types::{AccountId, MarketId, OrderId, Side};

/// What the live price is checked against before filling.
#[derive(Clone, Copy)]
pub(crate) enum PriceGuard {
    None,
    /// Market order: decline if the live price is more than `max_bps` worse than the price the
    /// client saw.
    Quote {
        quoted: Fixed,
        max_bps: i64,
    },
    /// Triggered limit: never fill worse than the limit.
    Limit(Fixed),
}

/// One fill to price, risk-check, and book.
pub(crate) struct Exec {
    pub account: AccountId,
    pub order_id: OrderId,
    pub market: MarketId,
    pub side: Side,
    pub qty: Fixed,
    pub reduce_only: bool,
    pub guard: PriceGuard,
    pub liquidation: bool,
}

/// A priced fill that has passed its price guard but is not yet booked.
struct Priced {
    qty: Fixed,
    delta: Fixed,
    price: Fixed,
    mid: Fixed,
    /// The FX index the fill converts to USD at; `None` for a USD market.
    fx: Option<Fixed>,
    spread_bps: i64,
    increases: bool,
}

impl Engine {
    /// Authorises the request, then fills a market order now or rests a limit or stop order,
    /// emitting acceptance or rejection.
    pub(crate) fn place_order(&mut self, req: &OrderRequest, events: &mut Vec<Event>) {
        let reject = |reason, events: &mut Vec<Event>| {
            events.push(Event::OrderRejected { account: req.account, client_id: req.client_id, reason })
        };
        let authorised = self.authorise(req.account, req.auth.as_ref(), |domain, auth| {
            order_hash(domain, req, auth.nonce, auth.expires_at)
        });
        if let Err(reason) = authorised.and_then(|()| self.validate_order(req)) {
            return reject(reason, events);
        }
        self.refresh_session(req.market, events);

        match req.kind {
            OrderKind::Market { quoted, max_slippage_bps } => {
                let house_bound = self.markets[&req.market].spec.max_slippage_bps;
                // The client may tighten the house bound but not widen it.
                let max_bps = max_slippage_bps.map_or(house_bound, |client| client.clamp(0, house_bound));
                let order_id = self.alloc_order_id();
                let exec = Exec {
                    account: req.account,
                    order_id,
                    market: req.market,
                    side: req.side,
                    qty: req.qty,
                    reduce_only: req.reduce_only,
                    guard: PriceGuard::Quote { quoted, max_bps },
                    liquidation: false,
                };
                match self.execute(exec) {
                    Ok(fill) => {
                        events.push(Event::OrderAccepted { account: req.account, client_id: req.client_id, order_id });
                        self.after_fill(fill, events);
                    }
                    Err(reason) => reject(reason, events),
                }
            }
            OrderKind::Limit { .. } | OrderKind::Stop { .. } => {
                let order_id = self.alloc_order_id();
                let order = RestingOrder {
                    order_id,
                    client_id: req.client_id,
                    market: req.market,
                    side: req.side,
                    qty: req.qty,
                    kind: req.kind,
                    reduce_only: req.reduce_only,
                };
                self.accounts.get_mut(&req.account).expect("validated").orders.insert(order_id, order);
                self.markets.get_mut(&req.market).expect("validated").resting.insert(order_id, req.account);
                self.record_order(req.account, order_id);
                events.push(Event::OrderAccepted { account: req.account, client_id: req.client_id, order_id });
                self.trigger_resting(req.market, events);
            }
        }
    }

    /// Authorises the request, then cancels a resting order on the client's request.
    pub(crate) fn cancel_order(
        &mut self,
        account: AccountId,
        order_id: OrderId,
        auth: Option<&RequestAuth>,
        events: &mut Vec<Event>,
    ) {
        if let Err(reason) = self.authorise(account, auth, |domain, auth| {
            cancel_hash(domain, account, order_id, auth.nonce, auth.expires_at)
        }) {
            events.push(Event::OrderRejected { account, client_id: 0, reason });
        } else if self.remove_order(account, order_id).is_some() {
            events.push(Event::OrderCancelled { account, order_id, reason: CancelReason::Client });
        } else {
            events.push(Event::OrderRejected { account, client_id: 0, reason: Reject::UnknownOrder });
        }
    }

    /// Fills resting orders whose trigger the live price has reached. While a market is closed, or
    /// its FX market has no price, nothing triggers, so the first fresh price after that fills
    /// everything that gapped through, at that price.
    pub(crate) fn trigger_resting(&mut self, market_id: MarketId, events: &mut Vec<Event>) {
        let market = &self.markets[&market_id];
        if !market.open || self.live_fx(market_id).is_err() {
            return;
        }
        let Ok(index) = compute_index(&market.quotes, &market.spec.price_rules, self.now) else { return };
        let resting: Vec<(OrderId, AccountId)> = market.resting.iter().map(|(o, a)| (*o, *a)).collect();
        for (order_id, account) in resting {
            self.trigger_order(order_id, account, index.mid, events);
        }
    }

    /// Prices and risk-checks one fill, then books it. Nothing is mutated on error.
    /// Error cases:
    /// - the market is closed, or has no valid index, or its FX market has no price
    /// - reduce-only with no opposite position, or new risk in a reduce-only market
    /// - new risk below the market's minimum order notional
    /// - a spread at this size that reaches the price itself
    /// - a live price beyond the slippage bound or the limit
    /// - not enough margin, or no house capacity, for the risk the fill adds
    pub(crate) fn execute(&mut self, exec: Exec) -> Result<Fill, Reject> {
        let priced = self.price(&exec)?;
        if priced.increases && !exec.liquidation {
            self.check_risk(&exec, &priced)?;
        }
        Ok(self.book(&exec, priced))
    }

    /// Records a fill and writes off any balance the account can no longer cover.
    pub(crate) fn after_fill(&mut self, fill: Fill, events: &mut Vec<Event>) {
        let account = fill.account;
        events.push(Event::Fill(fill));
        self.settle_bad_debt(account, events);
    }

    /// The next order id; ids are never reused.
    pub(crate) fn alloc_order_id(&mut self) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        id
    }

    /// Checks an order can be considered at all.
    /// Error cases:
    /// - protocol params not set yet
    /// - an unknown market
    /// - a qty, quoted price, or trigger price at or below zero or above `Fixed::MAX_INPUT`
    /// - an account that has never been credited
    fn validate_order(&self, req: &OrderRequest) -> Result<(), Reject> {
        let invalid = |reason: &str| Err(Reject::InvalidOrder { reason: reason.into() });
        if self.params.is_none() {
            return invalid("protocol params not set");
        }
        if !self.markets.contains_key(&req.market) {
            return Err(Reject::UnknownMarket);
        }
        if !req.qty.is_valid_input() {
            return invalid("qty out of range");
        }
        let price = match req.kind {
            OrderKind::Market { quoted, .. } => quoted,
            OrderKind::Limit { price } | OrderKind::Stop { price } => price,
        };
        if !price.is_valid_input() {
            return invalid("price out of range");
        }
        if !self.accounts.contains_key(&req.account) {
            return Err(Reject::InsufficientFunds { available: Fixed::ZERO });
        }
        Ok(())
    }

    /// Fills one resting order if `mid` has reached its trigger. A limit the spread keeps out of
    /// reach stays resting; any other failure cancels it.
    fn trigger_order(&mut self, order_id: OrderId, account: AccountId, mid: Fixed, events: &mut Vec<Event>) {
        let Some(order) = self.accounts.get(&account).and_then(|acct| acct.orders.get(&order_id)).cloned() else {
            return;
        };
        let (triggered, guard) = match (order.kind, order.side) {
            (OrderKind::Limit { price }, Side::Buy) => (mid <= price, PriceGuard::Limit(price)),
            (OrderKind::Limit { price }, Side::Sell) => (mid >= price, PriceGuard::Limit(price)),
            (OrderKind::Stop { price }, Side::Buy) => (mid >= price, PriceGuard::None),
            (OrderKind::Stop { price }, Side::Sell) => (mid <= price, PriceGuard::None),
            // `place_order` fills market orders immediately; they never rest.
            (OrderKind::Market { .. }, _) => unreachable!("market orders never rest"),
        };
        if !triggered {
            return;
        }
        let exec = Exec {
            account,
            order_id,
            market: order.market,
            side: order.side,
            qty: order.qty,
            reduce_only: order.reduce_only,
            guard,
            liquidation: false,
        };
        match self.execute(exec) {
            Ok(fill) => {
                self.remove_order(account, order_id);
                self.after_fill(fill, events);
            }
            Err(Reject::Slippage { .. }) => {}
            Err(_) => {
                self.remove_order(account, order_id);
                events.push(Event::OrderCancelled { account, order_id, reason: CancelReason::Unfillable });
            }
        }
    }

    /// The live price for a fill and the slice of it that adds risk. Notional bounds are in USD; the
    /// conversions round so the minimum and the size spread are never less than exact.
    /// Error cases:
    /// - the market is closed, or has no valid index, or its FX market has no price
    /// - reduce-only with no opposite position, or new risk in a reduce-only market
    /// - new risk below the market's minimum order notional
    /// - a spread at this size that reaches the price itself
    /// - a live price beyond the slippage bound or the limit
    fn price(&self, exec: &Exec) -> Result<Priced, Reject> {
        let params = self.params.as_ref().expect("callers check params are set");
        let market = &self.markets[&exec.market];
        if !market.open {
            return Err(Reject::MarketClosed);
        }
        let mid = compute_index(&market.quotes, &market.spec.price_rules, self.now)
            .map_err(|error| Reject::NoPrice { error })?
            .mid;
        let fx = self.live_fx(exec.market)?;
        let currency = market.spec.quote_currency;
        let usd = |amount, rounding| currency.to_usd(amount, fx, rounding);
        let acct = &self.accounts[&exec.account];
        let pos = acct.position(exec.market);

        let qty = if exec.reduce_only {
            if pos.qty.is_zero() || pos.qty.signum() == exec.side.sign() {
                return Err(Reject::ReduceOnly);
            }
            exec.qty.min(pos.qty.abs())
        } else {
            exec.qty
        };
        let delta = exec.side.signed(qty);
        let (grow_from, grow_to) = growth(pos.qty, delta);
        let increases = grow_to > grow_from;
        if increases && self.is_reduce_only(market) {
            return Err(Reject::ReduceOnly);
        }
        if increases && usd(qty.mul(mid), Rounding::Down) < market.spec.min_order_notional {
            return Err(Reject::InvalidOrder { reason: "below minimum order notional".into() });
        }

        let stress = &params.stress_levels[market.stress.level];
        let toxicity_bps = if exec.liquidation { 0 } else { params.toxicity_spread_bps(acct.toxicity_bps) };
        // The spread per unit with every charge rounded as `charge` says. Widening the growth slice
        // outwards can only add to its cost, and dividing the size cost by qty (not by notional) keeps
        // full precision.
        let spread = |charge: Rounding| {
            let (from, to) =
                (usd(grow_from.mul(mid), Rounding::Down), usd(grow_to.mul_rounded(mid, charge), Rounding::Up));
            let size_cost_usd = staircase(&market.spec.spread_tiers, from, to, charge)
                .mul_bps_rounded(stress.size_spread_multiplier_bps, charge);
            let size_cost = currency.from_usd(size_cost_usd, fx, Rounding::Up);
            mid.mul_bps_rounded(stress.min_spread_bps + toxicity_bps, charge) + size_cost.div_rounded(qty, charge)
        };
        // The truncated spread decides whether a spread reaches the price. From `V10` the spread rounds
        // up, and a sell's stops a unit short of the price, so rounding never makes it reach the price.
        if spread(Rounding::TowardZero) >= mid {
            return Err(Reject::InvalidOrder { reason: "spread at this size reaches the price".into() });
        }
        let rounded = spread(self.charge_rounding());
        let adjust = if exec.side == Side::Sell { rounded.min(mid - Fixed::from_raw(1)) } else { rounded };
        let price = mid + adjust.mul_int(exec.side.sign());

        match exec.guard {
            PriceGuard::None => {}
            PriceGuard::Quote { quoted, max_bps } => {
                let adverse = (price - quoted).mul_int(exec.side.sign());
                if adverse > quoted.mul_bps(max_bps) {
                    return Err(Reject::Slippage { quoted, live: price, slippage_bps: adverse.bps_of(quoted) });
                }
            }
            PriceGuard::Limit(limit) => {
                if (price - limit).mul_int(exec.side.sign()).is_positive() {
                    return Err(Reject::Slippage { quoted: limit, live: price, slippage_bps: 0 });
                }
            }
        }
        Ok(Priced { qty, delta, price, mid, fx, spread_bps: adjust.bps_of(mid), increases })
    }

    /// Checks a fill that adds risk against the account's margin and the house's exposure caps, in
    /// USD: realised PnL converts rounded down, and exposure rounded up.
    /// Error cases:
    /// - the account's equity after the fill would be below its margin requirement
    /// - the fill would push the house's net exposure past the market cap or the total cap
    fn check_risk(&self, exec: &Exec, priced: &Priced) -> Result<(), Reject> {
        let params = self.params.as_ref().expect("callers check params are set");
        let market = &self.markets[&exec.market];
        let usd = |amount, rounding| market.spec.quote_currency.to_usd(amount, priced.fx, rounding);

        let mut sim = self.accounts[&exec.account].clone();
        let version = self.protocol_version;
        let realised = sim.positions.entry(exec.market).or_default().apply(priced.delta, priced.price, version);
        let realised = usd(realised, Rounding::Down);
        sim.balance += realised - profit_fee(realised, params.profit_fee_bps, version);
        let (equity, required) = self.equity_and_margin(&sim, Some((exec.market, priced.mid, priced.fx)));
        if equity < required {
            return Err(Reject::InsufficientMargin { equity, required });
        }

        let nav = self.house_nav();
        let before = usd(market.client_qty.abs().mul(priced.mid), Rounding::Up);
        let after = usd((market.client_qty + priced.delta).abs().mul(priced.mid), Rounding::Up);
        if after > before {
            // `house_gross_exposure` values this market at its marks; swap that term for `after`.
            let exposure = |mark: Fixed| market.usd_at_mark(market.client_qty.abs().mul(mark), Rounding::Up);
            let at_mark = market.mark.map_or(Fixed::ZERO, exposure);
            let total_after = self.house_gross_exposure() - at_mark + after;
            if after > self.market_cap(&market.spec, nav) || total_after > self.total_cap(params, nav) {
                return Err(Reject::HouseCapacity);
            }
        }
        Ok(())
    }

    /// Books a priced fill: the position, the market aggregates, realised PnL in USD (rounded down)
    /// and the profit fee against house cash, and a markout to score the flow later.
    fn book(&mut self, exec: &Exec, priced: Priced) -> Fill {
        let params = self.params.as_ref().expect("callers check params are set");
        let fee_bps = params.profit_fee_bps;
        let version = self.protocol_version;
        let markout_due = self.now + params.toxicity.horizon_ms;
        let market = self.markets.get_mut(&exec.market).expect("priced against this market");
        let acct = self.accounts.get_mut(&exec.account).expect("priced for this account");

        let pos = acct.positions.entry(exec.market).or_default();
        market.client_qty -= pos.qty;
        market.client_cost -= pos.cost();
        let realised = pos.apply(priced.delta, priced.price, version);
        let realised_pnl = market.spec.quote_currency.to_usd(realised, priced.fx, Rounding::Down);
        market.client_qty += pos.qty;
        market.client_cost += pos.cost();
        let position = pos.qty;
        if position.is_zero() {
            acct.positions.remove(&exec.market);
        }
        let fee = profit_fee(realised_pnl, fee_bps, version);
        acct.balance += realised_pnl - fee;
        self.house_cash -= realised_pnl - fee;

        self.record_position(exec.account, exec.market);
        if !exec.liquidation {
            let quote = self.markets[&exec.market].spec.quote_currency;
            let notional = quote.to_usd(priced.qty.mul(priced.price), priced.fx, Rounding::Down);
            let (account, market, side, price) = (exec.account, exec.market, exec.side, priced.price);
            self.record_markout(Markout { due: markout_due, account, market, side, price, notional });
        }
        Fill {
            account: exec.account,
            order_id: exec.order_id,
            market: exec.market,
            side: exec.side,
            qty: priced.qty,
            price: priced.price,
            index: priced.mid,
            spread_bps: priced.spread_bps,
            realised_pnl,
            fx_index: priced.fx,
            profit_fee: fee,
            position,
            liquidation: exec.liquidation,
        }
    }

    /// How a spread charge or a margin requirement rounds: up from `V10`, so a trader never pays or
    /// posts less than the exact amount, and toward zero before.
    pub(crate) fn charge_rounding(&self) -> Rounding {
        if self.protocol_version >= ProtocolVersion::V10 { Rounding::Up } else { Rounding::TowardZero }
    }

    /// Removes a resting order from its account and its market's book.
    fn remove_order(&mut self, account: AccountId, order_id: OrderId) -> Option<RestingOrder> {
        let order = self.accounts.get_mut(&account)?.orders.remove(&order_id)?;
        if let Some(market) = self.markets.get_mut(&order.market) {
            market.resting.remove(&order_id);
        }
        self.record_order(account, order_id);
        Some(order)
    }
}

/// The slice of a trade that grows |position|, as `(from, to)` absolute quantities: the whole of
/// an increase, the new side of a flip, and nothing for a pure reduction.
fn growth(current: Fixed, delta: Fixed) -> (Fixed, Fixed) {
    let new = current + delta;
    if current.is_zero() || current.signum() == delta.signum() {
        (current.abs(), new.abs())
    } else if !new.is_zero() && new.signum() == delta.signum() {
        (Fixed::ZERO, new.abs())
    } else {
        (Fixed::ZERO, Fixed::ZERO)
    }
}

/// Losers pay nothing; winners pay `fee_bps` of realised profit, rounded up from `V5` and truncated
/// before it.
fn profit_fee(realised: Fixed, fee_bps: i64, version: ProtocolVersion) -> Fixed {
    if !realised.is_positive() {
        Fixed::ZERO
    } else if version >= ProtocolVersion::V5 {
        realised.mul_bps_rounded(fee_bps, Rounding::Up)
    } else {
        realised.mul_bps(fee_bps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(n: i64) -> Fixed {
        Fixed::from_int(n)
    }

    #[test]
    fn growth_covers_increase_flip_and_reduce() {
        assert_eq!(growth(f(0), f(3)), (f(0), f(3)));
        assert_eq!(growth(f(2), f(3)), (f(2), f(5)));
        // Long 2, sell 5: the 3 short is new risk.
        assert_eq!(growth(f(2), f(-5)), (f(0), f(3)));
        assert_eq!(growth(f(5), f(-2)), (f(0), f(0)));
        assert_eq!(growth(f(5), f(-5)), (f(0), f(0)));
    }

    #[test]
    fn profit_fee_only_on_gains() {
        for version in [ProtocolVersion::V2, ProtocolVersion::V5] {
            assert_eq!(profit_fee(f(100), 1_000, version), f(10));
            assert_eq!(profit_fee(f(-100), 1_000, version), f(0));
            assert_eq!(profit_fee(f(0), 1_000, version), f(0));
        }
    }

    #[test]
    fn a_fractional_profit_fee_rounds_up() {
        let realised = Fixed::from_raw(9);
        assert_eq!(profit_fee(realised, 1_000, ProtocolVersion::V5), Fixed::from_raw(1));
        assert_eq!(profit_fee(realised, 1_000, ProtocolVersion::V2), Fixed::ZERO);
        // The highest fee the engine accepts, 99.99%, takes the whole profit and no more.
        assert_eq!(profit_fee(realised, 9_999, ProtocolVersion::V5), realised);
    }
}
