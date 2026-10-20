//! Orders, pricing, fills and close-outs.
//!
//! Every fill is against the house at the live index mid plus a spread made
//! of three parts:
//! - the stress level's minimum spread (zero in normal conditions);
//! - the account's toxicity add-on (zero for normal flow);
//! - the size spread, charged only on the slice of the trade that grows the
//!   account's absolute position through the market's size bands.
//!
//! Normal trades therefore fill at the mid.

use crate::account::RestingOrder;
use crate::command::{OrderKind, OrderRequest};
use crate::engine::{Engine, Markout};
use crate::event::{CancelReason, Event, Fill, Reject};
use crate::market::staircase;
use crate::price::compute_index;
use crate::types::{AccountId, MarketId, OrderId, Side};
use crate::Fixed;

/// What the live price is checked against before filling.
#[derive(Clone, Copy)]
enum PriceGuard {
    None,
    /// Market order: decline if the live price is more than `max_bps`
    /// worse than the price the client saw.
    Quote { quoted: Fixed, max_bps: i64 },
    /// Triggered limit: never fill worse than the limit.
    Limit(Fixed),
}

struct Exec {
    account: AccountId,
    order_id: OrderId,
    market: MarketId,
    side: Side,
    qty: Fixed,
    reduce_only: bool,
    guard: PriceGuard,
    liquidation: bool,
}

impl Engine {
    pub(crate) fn place_order(&mut self, req: &OrderRequest, ev: &mut Vec<Event>) {
        let reject = |reason, ev: &mut Vec<Event>| {
            ev.push(Event::OrderRejected { account: req.account, client_id: req.client_id, reason })
        };
        if self.params.is_none() {
            return reject(Reject::InvalidOrder { reason: "protocol params not set".into() }, ev);
        }
        let Some(ms) = self.markets.get(&req.market) else {
            return reject(Reject::UnknownMarket, ev);
        };
        if !req.qty.is_positive() {
            return reject(Reject::InvalidOrder { reason: "qty must be positive".into() }, ev);
        }
        if !self.accounts.contains_key(&req.account) {
            return reject(Reject::InsufficientFunds { available: Fixed::ZERO }, ev);
        }
        let max_slippage = ms.spec.max_slippage_bps;
        self.refresh_session(req.market, ev);

        match req.kind {
            OrderKind::Market { quoted, max_slippage_bps } => {
                if !quoted.is_positive() {
                    return reject(Reject::InvalidOrder { reason: "quoted price must be positive".into() }, ev);
                }
                // The client may tighten the house bound but not widen it.
                let max_bps = max_slippage_bps.map_or(max_slippage, |c| c.clamp(0, max_slippage));
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
                        ev.push(Event::OrderAccepted { account: req.account, client_id: req.client_id, order_id });
                        self.after_fill(fill, ev);
                    }
                    Err(reason) => reject(reason, ev),
                }
            }
            OrderKind::Limit { price } | OrderKind::Stop { price } => {
                if !price.is_positive() {
                    return reject(Reject::InvalidOrder { reason: "trigger price must be positive".into() }, ev);
                }
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
                self.accounts.get_mut(&req.account).expect("checked").orders.insert(order_id, order);
                self.markets.get_mut(&req.market).expect("checked").resting.insert(order_id, req.account);
                ev.push(Event::OrderAccepted { account: req.account, client_id: req.client_id, order_id });
                self.trigger_resting(req.market, ev);
            }
        }
    }

    pub(crate) fn cancel_order(&mut self, account: AccountId, order_id: OrderId, ev: &mut Vec<Event>) {
        if self.remove_order(account, order_id).is_some() {
            ev.push(Event::OrderCancelled { account, order_id, reason: CancelReason::Client });
        } else {
            ev.push(Event::OrderRejected { account, client_id: 0, reason: Reject::UnknownOrder });
        }
    }

    /// Fills resting orders whose trigger the live price has reached. While a
    /// market is closed nothing triggers, so the first fresh price after the
    /// open fills everything that gapped through, at that price.
    pub(crate) fn trigger_resting(&mut self, m: MarketId, ev: &mut Vec<Event>) {
        let ms = &self.markets[&m];
        if !ms.open {
            return;
        }
        let Ok(index) = compute_index(&ms.quotes, &ms.spec.price_rules, self.now) else { return };
        let mid = index.mid;
        let resting: Vec<(OrderId, AccountId)> = ms.resting.iter().map(|(o, a)| (*o, *a)).collect();

        for (order_id, account) in resting {
            let Some(order) = self.accounts.get(&account).and_then(|a| a.orders.get(&order_id)).cloned() else {
                continue;
            };
            let (triggered, guard) = match (order.kind, order.side) {
                (OrderKind::Limit { price }, Side::Buy) => (mid <= price, PriceGuard::Limit(price)),
                (OrderKind::Limit { price }, Side::Sell) => (mid >= price, PriceGuard::Limit(price)),
                (OrderKind::Stop { price }, Side::Buy) => (mid >= price, PriceGuard::None),
                (OrderKind::Stop { price }, Side::Sell) => (mid <= price, PriceGuard::None),
                (OrderKind::Market { .. }, _) => unreachable!("market orders never rest"),
            };
            if !triggered {
                continue;
            }
            let exec = Exec {
                account,
                order_id,
                market: m,
                side: order.side,
                qty: order.qty,
                reduce_only: order.reduce_only,
                guard,
                liquidation: false,
            };
            match self.execute(exec) {
                Ok(fill) => {
                    self.remove_order(account, order_id);
                    self.after_fill(fill, ev);
                }
                // Mid reached the limit but spread keeps the live price worse; keep resting.
                Err(Reject::Slippage { .. }) => {}
                Err(_) => {
                    self.remove_order(account, order_id);
                    ev.push(Event::OrderCancelled { account, order_id, reason: CancelReason::Unfillable });
                }
            }
        }
    }

    /// Account close-out: while equity is below the close-out level
    /// of the margin requirement, close the largest losing position that can
    /// trade right now.
    pub(crate) fn check_closeouts(&mut self, m: MarketId, ev: &mut Vec<Event>) {
        let level_bps = self.params.as_ref().expect("checked by caller").closeout_level_bps;
        let accounts: Vec<AccountId> = self
            .accounts
            .iter()
            .filter(|(_, a)| a.positions.contains_key(&m))
            .map(|(id, _)| *id)
            .collect();

        for account in accounts {
            let mut announced = false;
            loop {
                let acct = &self.accounts[&account];
                let (equity, margin) = self.equity_and_margin(acct, None);
                if margin.is_zero() || equity >= margin.mul_bps(level_bps) {
                    break;
                }
                let worst = acct
                    .positions
                    .iter()
                    .filter(|(pm, _)| self.tradeable_now(**pm))
                    .filter_map(|(pm, pos)| {
                        let mark = self.markets[pm].mark?;
                        Some((pos.unrealised(mark), *pm, *pos))
                    })
                    .min_by_key(|(upnl, pm, _)| (*upnl, *pm));
                let Some((_, pm, pos)) = worst else { break };
                if !announced {
                    ev.push(Event::CloseOut { account, equity, margin });
                    announced = true;
                }
                let side = if pos.qty.is_positive() { Side::Sell } else { Side::Buy };
                let order_id = self.alloc_order_id();
                let exec = Exec {
                    account,
                    order_id,
                    market: pm,
                    side,
                    qty: pos.qty.abs(),
                    reduce_only: true,
                    guard: PriceGuard::None,
                    liquidation: true,
                };
                match self.execute(exec) {
                    Ok(fill) => self.after_fill(fill, ev),
                    Err(_) => break,
                }
            }
            self.settle_bad_debt(account, ev);
        }
    }

    fn tradeable_now(&self, m: MarketId) -> bool {
        let ms = &self.markets[&m];
        ms.open && compute_index(&ms.quotes, &ms.spec.price_rules, self.now).is_ok()
    }

    /// Prices and risk-checks a fill, then books it. Nothing is mutated on error.
    fn execute(&mut self, x: Exec) -> Result<Fill, Reject> {
        let params = self.params.as_ref().expect("checked by caller");
        let ms = &self.markets[&x.market];
        if !ms.open {
            return Err(Reject::MarketClosed);
        }
        let index = compute_index(&ms.quotes, &ms.spec.price_rules, self.now).map_err(Reject::NoPrice)?;
        let mid = index.mid;
        let acct = &self.accounts[&x.account];
        let pos = acct.position(x.market);

        let mut qty = x.qty;
        if x.reduce_only {
            if pos.qty.is_zero() || pos.qty.signum() == x.side.sign() {
                return Err(Reject::ReduceOnly);
            }
            qty = qty.min(pos.qty.abs());
        }
        let delta = x.side.signed(qty);
        let new_qty = pos.qty + delta;

        // The slice of the trade that grows |position|.
        let (grow_from, grow_to) = if pos.qty.is_zero() || pos.qty.signum() == delta.signum() {
            (pos.qty.abs(), new_qty.abs())
        } else if !new_qty.is_zero() && new_qty.signum() == delta.signum() {
            (Fixed::ZERO, new_qty.abs())
        } else {
            (Fixed::ZERO, Fixed::ZERO)
        };
        let increases = grow_to > grow_from;
        if increases && ms.reduce_only {
            return Err(Reject::ReduceOnly);
        }
        let trade_notional = qty.mul(mid);
        if increases && trade_notional < ms.spec.min_order_notional {
            return Err(Reject::InvalidOrder { reason: "below minimum order notional".into() });
        }

        // Price.
        let stress = &params.stress_levels[ms.stress.level];
        let size_cost = staircase(&ms.spec.spread_tiers, grow_from.mul(mid), grow_to.mul(mid))
            .mul_bps(stress.size_spread_multiplier_bps);
        let toxicity_bps = if x.liquidation { 0 } else { params.toxicity_spread_bps(acct.toxicity_bps) };
        // Per-unit price adjustment; dividing the size cost by qty (not by
        // notional) keeps full precision.
        let adjust = mid.mul_bps(stress.min_spread_bps + toxicity_bps) + size_cost.div(qty);
        let price = mid + adjust.mul_int(x.side.sign());
        let spread_bps = (price - mid).abs().bps_of(mid);

        match x.guard {
            PriceGuard::None => {}
            PriceGuard::Quote { quoted, max_bps } => {
                let adverse = (price - quoted).mul_int(x.side.sign());
                if adverse > quoted.mul_bps(max_bps) {
                    return Err(Reject::Slippage { quoted, live: price, slippage_bps: adverse.bps_of(quoted) });
                }
            }
            PriceGuard::Limit(limit) => {
                if (price - limit).mul_int(x.side.sign()).is_positive() {
                    return Err(Reject::Slippage { quoted: limit, live: price, slippage_bps: 0 });
                }
            }
        }

        // Risk: only trades that add risk are checked. Reducing is always allowed.
        if increases && !x.liquidation {
            let mut sim = acct.clone();
            let realised = sim.positions.entry(x.market).or_default().apply(delta, price);
            sim.balance += realised - profit_fee(realised, params.profit_fee_bps);
            let (equity, required) = self.equity_and_margin(&sim, Some((x.market, mid)));
            if equity < required {
                return Err(Reject::InsufficientMargin { equity, required });
            }

            let nav = self.house_nav();
            let before = ms.client_qty.abs().mul(mid);
            let after = (ms.client_qty + delta).abs().mul(mid);
            if after > before {
                let total_after = self.house_gross_exposure() - before + after;
                if after > nav.mul_bps(ms.spec.house_cap_bps) || total_after > nav.mul_bps(params.house_total_cap_bps) {
                    return Err(Reject::HouseCapacity);
                }
            }
        }

        // Book it.
        let fee_bps = params.profit_fee_bps;
        let toxicity_horizon = params.toxicity.horizon_ms;
        let ms = self.markets.get_mut(&x.market).expect("checked");
        let acct = self.accounts.get_mut(&x.account).expect("checked");
        let pos = acct.positions.entry(x.market).or_default();
        ms.client_qty -= pos.qty;
        ms.client_cost -= pos.cost();
        let realised_pnl = pos.apply(delta, price);
        ms.client_qty += pos.qty;
        ms.client_cost += pos.cost();
        let position = pos.qty;
        if position.is_zero() {
            acct.positions.remove(&x.market);
        }
        let fee = profit_fee(realised_pnl, fee_bps);
        acct.balance += realised_pnl - fee;
        self.house_cash -= realised_pnl - fee;

        if !x.liquidation {
            self.markouts.push_back(Markout {
                due: self.now + toxicity_horizon,
                account: x.account,
                market: x.market,
                side: x.side,
                price,
            });
        }

        Ok(Fill {
            account: x.account,
            order_id: x.order_id,
            market: x.market,
            side: x.side,
            qty,
            price,
            index: mid,
            spread_bps,
            realised_pnl,
            profit_fee: fee,
            position,
            liquidation: x.liquidation,
        })
    }

    fn after_fill(&mut self, fill: Fill, ev: &mut Vec<Event>) {
        let account = fill.account;
        ev.push(Event::Fill(fill));
        self.settle_bad_debt(account, ev);
    }

    /// Negative balance protection: once an account is flat, any negative
    /// balance is written off against the house.
    fn settle_bad_debt(&mut self, account: AccountId, ev: &mut Vec<Event>) {
        let Some(acct) = self.accounts.get_mut(&account) else { return };
        if acct.positions.is_empty() && acct.balance.is_negative() {
            let amount = -acct.balance;
            acct.balance = Fixed::ZERO;
            self.house_cash -= amount;
            ev.push(Event::BadDebt { account, amount });
        }
    }

    fn remove_order(&mut self, account: AccountId, order_id: OrderId) -> Option<RestingOrder> {
        let order = self.accounts.get_mut(&account)?.orders.remove(&order_id)?;
        if let Some(ms) = self.markets.get_mut(&order.market) {
            ms.resting.remove(&order_id);
        }
        Some(order)
    }

    fn alloc_order_id(&mut self) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        id
    }
}

/// Losers pay nothing; winners pay `fee_bps` of realised profit.
fn profit_fee(realised: Fixed, fee_bps: i64) -> Fixed {
    if realised.is_positive() { realised.mul_bps(fee_bps) } else { Fixed::ZERO }
}
