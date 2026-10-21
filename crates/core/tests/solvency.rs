//! Solvency: house exposure caps with dollar floors, and a house at zero or negative NAV, end to end
//! through `Engine::apply`.

mod common;

use common::{BTC, ETH, Harness, acct, f, fills, market, params, rejection};
use engine_core::Fixed;
use engine_core::command::{AdminCommand, Command};
use engine_core::event::{Event, Reject};
use engine_core::market::MarketSpec;
use engine_core::params::ProtocolParams;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::types::{AssetClass, MarketId, Side};

#[test]
fn a_house_with_no_capital_opens_trading_up_to_the_market_floor() {
    let mut h = Harness::zero_house(ProtocolVersion::V6);
    let (a, b) = (acct(1), acct(2));
    h.deposit(a, "1000000");
    h.deposit(b, "1000000");
    h.price(BTC, "100000");
    // With no floor, a house with no capital takes on no risk.
    assert_eq!(rejection(&h.market_order(a, BTC, Side::Buy, "0.001", "100000")), &Reject::HouseCapacity);

    set_floors(&mut h, &[(BTC, "500000")], "2000000");
    assert_eq!(h.engine.house_nav(), Fixed::ZERO);
    assert_eq!(fills(&h.market_order(a, BTC, Side::Buy, "5", "100000")).len(), 1);
    let ev = h.market_order(a, BTC, Side::Buy, "0.000000001", "100000");
    assert_eq!(rejection(&ev), &Reject::HouseCapacity);
    // Flow that shrinks the house's exposure is never capped.
    assert_eq!(fills(&h.market_order(b, BTC, Side::Sell, "8", "100000")).len(), 1);
    h.assert_conserved(Fixed::ZERO);
}

#[test]
fn the_total_floor_binds_across_markets() {
    let mut h = Harness::zero_house(ProtocolVersion::V6);
    let a = acct(1);
    h.deposit(a, "1000000");
    set_floors(&mut h, &[(BTC, "500000"), (ETH, "500000")], "600000");
    h.price(BTC, "100000");
    h.price(ETH, "4000");
    assert_eq!(fills(&h.market_order(a, BTC, Side::Buy, "4", "100000")).len(), 1);
    // 300,000 of ETH fits ETH's floor, but 700,000 in total is past 600,000; 200,000 fits exactly.
    assert_eq!(rejection(&h.market_order(a, ETH, Side::Buy, "75", "4000")), &Reject::HouseCapacity);
    assert_eq!(fills(&h.market_order(a, ETH, Side::Buy, "50", "4000")).len(), 1);
}

#[test]
fn a_cap_is_the_larger_of_its_nav_share_and_its_floor_from_v6() {
    // 100m of NAV caps BTC at a 50m share and the total at a 200m share. Each case buys `qty` BTC
    // under a BTC floor and a total floor.
    let fills = |mut h: Harness, qty: &str, btc_floor: &str, total_floor: &str| {
        let whale = acct(1);
        h.deposit(whale, "100000000");
        set_floors(&mut h, &[(BTC, btc_floor)], total_floor);
        h.price(BTC, "100000");
        let ev = h.market_order(whale, BTC, Side::Buy, qty, "100100");
        match &ev[..] {
            [Event::OrderAccepted { .. }, Event::Fill(_)] => true,
            [Event::OrderRejected { reason: Reject::HouseCapacity, .. }] => false,
            _ => panic!("{ev:?}"),
        }
    };
    // A 70m floor above the BTC share lets 60m in.
    assert!(fills(latest(), "600", "70000000", "0"));
    // A share above a 10m floor wins: 40m fits the BTC share, and the total share.
    assert!(fills(latest(), "400", "10000000", "0"));
    assert!(fills(latest(), "400", "0", "10000000"));
    // The 50m BTC share still refuses 60m under a 10m floor.
    assert!(!fills(latest(), "600", "10000000", "0"));
    // Before `V6` floors are ignored.
    assert!(!fills(Harness::v5(), "600", "70000000", "0"));
}

#[test]
fn a_negative_house_halts_nothing() {
    let (a, b) = (acct(1), acct(2));
    let mut h = Harness::zero_house(ProtocolVersion::V6);
    h.deposit(a, "100000");
    h.deposit(b, "100000");
    set_floors(&mut h, &[(BTC, "1000000"), (ETH, "1000000")], "3000000");
    h.price(BTC, "100000");
    // Utilisation is measured against the floor: 200,000 of a 1m cap is below the 80% trigger.
    assert_eq!(fills(&h.market_order(a, BTC, Side::Buy, "2", "100000")).len(), 1);

    // After the move window, BTC rises 10%: the house owes `a` 20,000 it does not have.
    h.advance(61_000);
    h.price(BTC, "110000");
    h.price(ETH, "4000");
    assert_eq!(h.engine.house_nav(), f("-20000"));
    assert_eq!(h.engine.markets()[&BTC].stress.level, 0);

    // New risk, withdrawals, and a winning close all go ahead.
    assert_eq!(fills(&h.market_order(b, ETH, Side::Buy, "25", "4000")).len(), 1);
    let ev = h.cmd(Command::Withdraw { account: b, amount: f("1000"), auth: None });
    assert_eq!(ev, [Event::WithdrawalApproved { account: b, amount: f("1000"), nonce: 1 }]);
    assert_eq!(fills(&h.market_order(a, BTC, Side::Sell, "2", "110000")).len(), 1);
    assert!(h.engine.house_cash().is_negative());
    let winnings = h.balance(a);
    let ev = h.cmd(Command::Withdraw { account: a, amount: winnings, auth: None });
    assert_eq!(ev, [Event::WithdrawalApproved { account: a, amount: winnings, nonce: 1 }]);
    h.assert_conserved(f("1000") + winnings);

    // Before `V6` the same zero house refuses all new risk, and reads its caps as fully used.
    let mut h = Harness::zero_house(ProtocolVersion::V5);
    h.deposit(a, "100000");
    set_floors(&mut h, &[(BTC, "1000000"), (ETH, "1000000")], "3000000");
    h.price(BTC, "100000");
    assert_eq!(rejection(&h.market_order(a, BTC, Side::Buy, "2", "100000")), &Reject::HouseCapacity);
    assert_eq!(h.engine.markets()[&BTC].stress.level, 1);
}

#[test]
fn cap_floors_outside_zero_to_the_input_bound_are_refused() {
    let mut h = latest();
    for floor in [Fixed::from_raw(-1), Fixed::MAX_INPUT + Fixed::from_raw(1)] {
        let listing = AdminCommand::UpsertMarket(MarketSpec { house_cap_floor: floor, ..crypto(BTC) });
        let parameters = AdminCommand::SetParams(ProtocolParams { house_total_cap_floor: floor, ..params() });
        for cmd in [listing, parameters] {
            let ev = h.cmd(Command::Admin(cmd.clone()));
            assert!(matches!(&ev[..], [Event::AdminRejected { .. }]), "{cmd:?}: {ev:?}");
        }
    }
    assert_eq!(h.engine.markets()[&BTC].spec.house_cap_floor, Fixed::ZERO);
    set_floors(&mut h, &[(BTC, "1000000000")], "1000000000");
    assert_eq!(h.engine.markets()[&BTC].spec.house_cap_floor, Fixed::MAX_INPUT);
    assert_eq!(h.engine.params().unwrap().house_total_cap_floor, Fixed::MAX_INPUT);
}

#[test]
fn a_zero_cap_floor_is_left_out_of_a_listing_encoding() {
    let spec = crypto(BTC);
    assert!(serde_json::to_value(&spec).unwrap().get("house_cap_floor").is_none());
    let floored = MarketSpec { house_cap_floor: f("250000"), ..spec };
    let json = serde_json::to_value(&floored).unwrap();
    assert_eq!(json["house_cap_floor"], "250000");
    assert_eq!(serde_json::from_value::<MarketSpec>(json).unwrap(), floored);
}

// ── Helpers ──

/// Genesis under `V6`, with unsigned requests and 100m of house capital.
fn latest() -> Harness {
    Harness::v6()
}

/// The harness listing of a 24/7 crypto market at 10% margin.
fn crypto(id: MarketId) -> MarketSpec {
    market(id, if id == BTC { "BTC-USD" } else { "ETH-USD" }, AssetClass::Crypto, Schedule::AlwaysOpen, 1_000)
}

/// Relists each market with its cap floor and sets the total cap floor, all of which must apply.
fn set_floors(h: &mut Harness, markets: &[(MarketId, &str)], total: &str) {
    for (id, floor) in markets {
        h.admin(AdminCommand::UpsertMarket(MarketSpec { house_cap_floor: f(floor), ..crypto(*id) }));
    }
    h.admin(AdminCommand::SetParams(ProtocolParams { house_total_cap_floor: f(total), ..params() }));
}
