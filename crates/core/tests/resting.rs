//! Resting orders from `V13`: an account rests at most the parameters' cap, a resting order that may
//! add risk meets its market's minimum notional at its own price, and placing one checks only that
//! order against the live price.

mod common;

use common::events::{fills, rejection};
use common::stress::with_divergence_shares;
use common::{BTC, DE40, ETH, EURUSD, Harness, USDJPY, acct, de40, f, market, params};
use engine_core::command::{AdminCommand, Command, OrderKind, OrderRequest};
use engine_core::event::{CancelReason, Event, Reject};
use engine_core::market::{MarketSpec, QuoteCurrency};
use engine_core::params::{DEFAULT_MAX_RESTING_ORDERS, ProtocolParams};
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::types::{AccountId, AssetClass, MarketId, OrderId, Side};

/// A second market quoted in euros at EUR-USD.
const FR40: MarketId = 7;

#[test]
fn an_account_rests_dust_limits_up_to_the_default_cap_and_no_more() {
    // The issue's scenario: buy limits of one unit of precision at a price of one unit of precision.
    let mut h = Harness::v13();
    let (a, b) = (acct(1), acct(2));
    h.deposit(a, "1000");
    h.deposit(b, "1000");
    h.price(BTC, "100000");
    let ids: Vec<OrderId> = (0..DEFAULT_MAX_RESTING_ORDERS).map(|_| accepted(&dust_limit(&mut h, a, BTC))).collect();
    assert_eq!(rejection(&dust_limit(&mut h, a, BTC)), &Reject::TooManyOrders { max: DEFAULT_MAX_RESTING_ORDERS });
    assert_eq!(rejection(&dust_limit(&mut h, a, ETH)), &Reject::TooManyOrders { max: DEFAULT_MAX_RESTING_ORDERS });
    assert_eq!(h.engine.accounts()[&a].orders.len(), DEFAULT_MAX_RESTING_ORDERS);
    assert_eq!(h.engine.markets()[&BTC].resting.len(), DEFAULT_MAX_RESTING_ORDERS);

    // The cap is per account, and a cancel frees a place in any market.
    accepted(&dust_limit(&mut h, b, BTC));
    h.cmd(Command::CancelOrder { account: a, order_id: ids[0], auth: None });
    accepted(&dust_limit(&mut h, a, ETH));
    assert!(matches!(rejection(&dust_limit(&mut h, a, BTC)), Reject::TooManyOrders { .. }));
    // A market order is never capped.
    assert_eq!(fills(&h.market_order(a, BTC, Side::Buy, "0.001", "100000")).len(), 1);
}

#[test]
fn the_cap_counts_every_resting_kind_and_follows_the_parameters_in_force() {
    let mut h = Harness::v13();
    h.operator_admin(AdminCommand::SetParams(ProtocolParams {
        max_resting_orders: 2,
        ..with_divergence_shares(params())
    }));
    let a = acct(1);
    h.deposit(a, "100000");
    h.price(BTC, "100000");
    h.price(ETH, "3000");
    accepted(&h.order(a, BTC, Side::Buy, "0.1", OrderKind::Limit { price: f("90000") }));
    accepted(&h.order(a, ETH, Side::Buy, "1", OrderKind::Stop { price: f("3500") }));
    let reducing = reducing(a, ETH, "1", OrderKind::Stop { price: f("2500") });
    assert_eq!(rejection(&h.cmd(Command::PlaceOrder(reducing.clone()))), &Reject::TooManyOrders { max: 2 });

    // Lowering the cap keeps the orders already resting and refuses the next; raising it admits one.
    h.operator_admin(AdminCommand::SetParams(ProtocolParams {
        max_resting_orders: 1,
        ..with_divergence_shares(params())
    }));
    assert_eq!(h.engine.accounts()[&a].orders.len(), 2);
    assert_eq!(rejection(&h.cmd(Command::PlaceOrder(reducing.clone()))), &Reject::TooManyOrders { max: 1 });
    h.operator_admin(AdminCommand::SetParams(ProtocolParams {
        max_resting_orders: 3,
        ..with_divergence_shares(params())
    }));
    accepted(&h.cmd(Command::PlaceOrder(reducing)));
}

#[test]
fn the_cap_applies_from_the_seq_v13_activates() {
    let mut h = Harness::v12();
    // Before `V13` parameters may not name a cap, and the default is not applied.
    let capped = AdminCommand::SetParams(ProtocolParams { max_resting_orders: 1, ..params() });
    let events = h.operate(capped);
    assert!(
        matches!(&events[..], [Event::AdminRejected { reason }] if reason.contains("needs protocol V13")),
        "{events:?}"
    );
    let a = acct(1);
    h.deposit(a, "1000");
    h.price(BTC, "100000");
    (0..=DEFAULT_MAX_RESTING_ORDERS).for_each(|_| {
        accepted(&dust_limit(&mut h, a, BTC));
    });
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V13 });
    assert_eq!(rejection(&dust_limit(&mut h, a, BTC)), &Reject::TooManyOrders { max: DEFAULT_MAX_RESTING_ORDERS });
    assert_eq!(h.engine.accounts()[&a].orders.len(), DEFAULT_MAX_RESTING_ORDERS + 1);
    h.operator_admin(AdminCommand::SetParams(ProtocolParams {
        max_resting_orders: 1,
        ..with_divergence_shares(params())
    }));
}

#[test]
fn a_resting_order_that_may_add_risk_meets_the_minimum_notional_at_its_own_price() {
    let below = || Reject::InvalidOrder { reason: "below minimum order notional".into() };
    let mut h = Harness::v13();
    let mut eth = market(ETH, "ETH-USD", AssetClass::Crypto, Schedule::AlwaysOpen, 1_000);
    eth.min_order_notional = f("100");
    h.operator_admin(AdminCommand::UpsertMarket(eth.clone()));
    let a = acct(1);
    h.deposit(a, "100000");
    h.price(ETH, "3000");
    assert_eq!(rejection(&dust_limit(&mut h, a, ETH)), &below());
    // Each order is valued at its own price, not at ETH's mark of 3,000: 0.03 at 3,400 is 102 and
    // rests though it is 90 at the mark, and 0.04 at 2,400 is 96 and is refused though it is 120 at
    // the mark. 0.04 at 2,500 is exactly the minimum.
    accepted(&h.order(a, ETH, Side::Sell, "0.03", OrderKind::Limit { price: f("3400") }));
    assert_eq!(rejection(&h.order(a, ETH, Side::Buy, "0.04", OrderKind::Limit { price: f("2400") })), &below());
    accepted(&h.order(a, ETH, Side::Sell, "0.04", OrderKind::Stop { price: f("2500") }));
    // A reduce-only order never adds risk, so it rests at any size.
    let reducing = reducing(a, ETH, "0.01", OrderKind::Stop { price: f("2500") });
    accepted(&h.cmd(Command::PlaceOrder(reducing)));

    // Before `V13` the dust limit rests, and only a fill checks the minimum.
    let mut h = Harness::v12();
    h.operator_admin(AdminCommand::UpsertMarket(eth));
    h.deposit(a, "100000");
    h.price(ETH, "3000");
    accepted(&dust_limit(&mut h, a, ETH));
}

#[test]
fn a_resting_order_in_a_market_quoted_in_euros_meets_the_minimum_in_dollars() {
    let below = || Reject::InvalidOrder { reason: "below minimum order notional".into() };
    let euro = |id, symbol: &str| MarketSpec { id, symbol: symbol.into(), min_order_notional: f("100"), ..de40(500) };
    let mut h = Harness::v13();
    h.operator_admin(AdminCommand::UpsertMarket(market(EURUSD, "EUR-USD", AssetClass::Fx, Schedule::AlwaysOpen, 200)));
    h.operator_admin(AdminCommand::UpsertMarket(euro(DE40, "DE40")));
    let a = acct(1);
    h.deposit(a, "100000");
    // With no EUR-USD mark the notional has no dollar value, so an order that may add risk is refused.
    let unpriced = h.order(a, DE40, Side::Buy, "1", OrderKind::Limit { price: f("95") });
    assert_eq!(rejection(&unpriced), &Reject::NoFxPrice { fx_market: EURUSD });
    accepted(&h.cmd(Command::PlaceOrder(reducing(a, DE40, "0.001", OrderKind::Stop { price: f("90") }))));
    // A euro market listed after EUR-USD's mark converts at that mark before it has a price or an FX
    // mark of its own: at 1.1 dollars a euro, 1 at 95 euros is 104.5 dollars and rests, and 1 at 90
    // is 99.
    h.price(EURUSD, "1.1");
    h.operator_admin(AdminCommand::UpsertMarket(euro(FR40, "FR40")));
    assert_eq!(h.engine.markets()[&FR40].fx_mark, None);
    accepted(&h.order(a, FR40, Side::Buy, "1", OrderKind::Limit { price: f("95") }));
    assert_eq!(rejection(&h.order(a, FR40, Side::Buy, "1", OrderKind::Limit { price: f("90") })), &below());
}

#[test]
fn a_usd_jpy_order_is_valued_at_its_own_price_as_its_fill_would_be() {
    // USD-JPY is quoted in yen and converts at its own index, so a fill's dollar notional is its
    // qty, at whatever price it fills.
    let below = || Reject::InvalidOrder { reason: "below minimum order notional".into() };
    let mut h = Harness::v13();
    let usdjpy = market(USDJPY, "USD-JPY", AssetClass::Fx, Schedule::AlwaysOpen, 200);
    let yen = QuoteCurrency::Jpy { fx_market: USDJPY };
    let spec = MarketSpec { quote_currency: yen, min_order_notional: f("100"), ..usdjpy };
    h.operator_admin(AdminCommand::UpsertMarket(spec));
    let a = acct(1);
    h.deposit(a, "100000");
    // Before USD-JPY has a price, an order converts at its own price.
    accepted(&h.order(a, USDJPY, Side::Buy, "100", OrderKind::Limit { price: f("149") }));
    // At a mark of 150, a buy of 100 at 149 is 100 dollars, as its fill at 149 would be, and 99.9 is
    // below; a sell of 100 at 151 is 100 dollars, and 99.5 is below.
    h.price(USDJPY, "150");
    accepted(&h.order(a, USDJPY, Side::Buy, "100", OrderKind::Limit { price: f("149") }));
    assert_eq!(rejection(&h.order(a, USDJPY, Side::Buy, "99.9", OrderKind::Limit { price: f("149") })), &below());
    accepted(&h.order(a, USDJPY, Side::Sell, "100", OrderKind::Limit { price: f("151") }));
    assert_eq!(rejection(&h.order(a, USDJPY, Side::Sell, "99.5", OrderKind::Limit { price: f("151") })), &below());
}

#[test]
fn placing_a_resting_order_checks_only_that_order_from_v13() {
    for (mut h, fills_on_placement) in [(Harness::v12(), true), (Harness::v13(), false)] {
        let version = h.engine.protocol_version();
        let (a, b) = (acct(1), acct(2));
        h.deposit(a, "100000");
        h.deposit(b, "100000");
        // Under the 5bps stress spread a buy limit at the mid triggers but would fill above it, so
        // it keeps resting. With the floor back at zero it could fill at the mid.
        h.operator_admin(AdminCommand::SetStressFloor { market: Some(BTC), level: 1 });
        h.price(BTC, "100000");
        let held = accepted(&h.order(a, BTC, Side::Buy, "0.1", OrderKind::Limit { price: f("100000") }));
        h.operator_admin(AdminCommand::SetStressFloor { market: Some(BTC), level: 0 });

        // Another account's resting order fills the held limit only before `V13`.
        let ev = h.order(b, BTC, Side::Buy, "0.1", OrderKind::Limit { price: f("90000") });
        let filled: Vec<OrderId> = fills(&ev).iter().map(|fill| fill.order_id).collect();
        assert_eq!(filled == [held], fills_on_placement, "{version:?}: {ev:?}");
        // A marketable limit still fills as it is placed.
        let marketable = h.order(b, BTC, Side::Buy, "0.1", OrderKind::Limit { price: f("100010") });
        let order_id = accepted(&marketable);
        assert_eq!(fills(&marketable).iter().map(|fill| fill.order_id).collect::<Vec<_>>(), [order_id], "{version:?}");
        // From `V13` the next price fills the held limit.
        let ev = h.price(BTC, "100000");
        let filled: Vec<OrderId> = fills(&ev).iter().map(|fill| fill.order_id).collect();
        assert_eq!(filled == [held], !fills_on_placement, "{version:?}: {ev:?}");
    }
}

#[test]
fn a_placed_order_that_cannot_fill_is_cancelled_and_frees_its_place() {
    let mut h = Harness::v13();
    h.operator_admin(AdminCommand::SetParams(ProtocolParams {
        max_resting_orders: 1,
        ..with_divergence_shares(params())
    }));
    let a = acct(1);
    h.deposit(a, "100");
    h.price(BTC, "100000");
    // A stop the account cannot margin triggers as it is placed and is cancelled.
    let ev = h.order(a, BTC, Side::Buy, "1", OrderKind::Stop { price: f("99000") });
    let order_id = accepted(&ev);
    assert!(ev.contains(&Event::OrderCancelled { account: a, order_id, reason: CancelReason::Unfillable }), "{ev:?}");
    accepted(&dust_limit(&mut h, a, BTC));
}

// ── Helpers ──

/// A buy limit of one unit of precision at a price of one unit of precision.
fn dust_limit(h: &mut Harness, account: AccountId, market: MarketId) -> Vec<Event> {
    h.order(account, market, Side::Buy, "0.000000001", OrderKind::Limit { price: f("0.000000001") })
}

/// An unsigned reduce-only sell with client id 7.
fn reducing(account: AccountId, market: MarketId, qty: &str, kind: OrderKind) -> OrderRequest {
    OrderRequest { account, client_id: 7, market, side: Side::Sell, qty: f(qty), kind, reduce_only: true, auth: None }
}

/// The order id an `OrderAccepted` among `events` carries; panics if there is none.
fn accepted(events: &[Event]) -> OrderId {
    events
        .iter()
        .find_map(|e| match e {
            Event::OrderAccepted { order_id, .. } => Some(*order_id),
            _ => None,
        })
        .unwrap_or_else(|| panic!("expected an accepted order, got {events:?}"))
}
