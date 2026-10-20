//! Position accounting rounds in the house's favour from `V5`: dust adds cannot move an entry in the
//! holder's favour, and closing never realises more than the exact PnL, end to end through
//! `Engine::apply`.

mod common;

use common::{AAPL, BTC, ETH, Harness, acct, f, fills, market, ny, rejection};
use engine_core::Fixed;
use engine_core::auth::ClientAuth;
use engine_core::command::{AdminCommand, Command};
use engine_core::event::{Event, Fill, Reject};
use engine_core::market::{MarketSpec, Tier};
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::types::{AccountId, AssetClass, MarketId, Side};

/// An FX market with 1% margin and no size spread, so every fill is at the mid.
const EURUSD: MarketId = 4;
/// A market whose longs are bought at one unit of precision and then marked at the input ceiling.
const DUSTY: MarketId = 5;
const DUST: &str = "0.000000001";

#[test]
fn dust_buys_on_a_long_at_its_entry_price_realise_nothing_on_close() {
    let (mut h, a) = eurusd(Harness::v5(), "1.1");
    h.market_order(a, EURUSD, Side::Buy, "4500000", "1.1");
    (0..1_000).for_each(|_| assert_eq!(fill(h.market_order(a, EURUSD, Side::Buy, DUST, "1.1")).price, f("1.1")));
    assert_eq!(entry(&h, a), f("1.1"));
    let house = h.engine.house_cash();

    let close = fill(h.market_order(a, EURUSD, Side::Sell, "4500000.000001", "1.1"));
    assert_eq!((close.position, close.realised_pnl), (Fixed::ZERO, Fixed::ZERO));
    assert_eq!((h.balance(a), h.engine.house_cash()), (f("1000000"), house));
}

#[test]
fn dust_sells_on_a_short_at_its_entry_price_realise_nothing_on_close() {
    let (mut h, a) = eurusd(Harness::v5(), "1.1");
    h.market_order(a, EURUSD, Side::Sell, "4500000", "1.1");
    (0..1_000).for_each(|_| assert_eq!(fill(h.market_order(a, EURUSD, Side::Sell, DUST, "1.1")).price, f("1.1")));
    assert_eq!(entry(&h, a), f("1.1"));
    let house = h.engine.house_cash();

    let close = fill(h.market_order(a, EURUSD, Side::Buy, "4500000.000001", "1.1"));
    assert_eq!((close.position, close.realised_pnl), (Fixed::ZERO, Fixed::ZERO));
    assert_eq!((h.balance(a), h.engine.house_cash()), (f("1000000"), house));
}

#[test]
fn dust_adds_at_a_moved_price_never_beat_closing_without_them() {
    // The long is 100 units of precision above the new price and the short 100 below it, so each
    // exact average lands just inside the entry and rounds back onto it.
    for (side, moved) in [(Side::Buy, "1.0999999"), (Side::Sell, "1.1000001")] {
        let (mut h, adder) = eurusd(Harness::v5(), "1.1");
        let holder = acct(2);
        h.deposit(holder, "1000000");
        h.market_order(adder, EURUSD, side, "4500000", "1.1");
        h.market_order(holder, EURUSD, side, "4500000", "1.1");
        h.price(EURUSD, moved);
        (0..1_000).for_each(|_| {
            h.market_order(adder, EURUSD, side, DUST, moved);
        });
        assert_eq!(entry(&h, adder), f("1.1"), "{side:?}");

        let back = if side == Side::Buy { Side::Sell } else { Side::Buy };
        let with_adds = fill(h.market_order(adder, EURUSD, back, "4500000.000001", moved)).realised_pnl;
        let without = fill(h.market_order(holder, EURUSD, back, "4500000", moved)).realised_pnl;
        // Closing 4.5m at 0.0000001 against the holder loses 0.45; the dust adds lose a little more.
        assert_eq!((with_adds, without), (f("-0.450000001"), f("-0.45")), "{side:?}");
    }
}

#[test]
fn an_add_then_a_round_trip_at_one_price_realises_nothing() {
    let mut h = Harness::v5();
    let a = acct(1);
    h.deposit(a, "100000");
    h.price(BTC, "100000.123456789");
    let price = "100000.123456789";
    // 1.5 and 2.5 at this price are not whole units of precision, so a truncated average would
    // land one unit below the price.
    h.market_order(a, BTC, Side::Buy, "1.5", price);
    h.market_order(a, BTC, Side::Buy, "2.5", price);
    assert_eq!(entry(&h, a), f(price));
    // A reduction, a flip through the position, and the close of the new short.
    let realised: Vec<Fixed> = [(Side::Sell, "1"), (Side::Sell, "4"), (Side::Buy, "1")]
        .into_iter()
        .map(|(side, qty)| fill(h.market_order(a, BTC, side, qty, price)).realised_pnl)
        .collect();
    assert_eq!(realised, [Fixed::ZERO; 3]);
    assert!(h.engine.accounts()[&a].positions.is_empty());
    assert_eq!(h.balance(a), f("100000"));
}

#[test]
fn the_rounding_rule_changes_at_the_seq_v5_activates() {
    let (mut h, a) = eurusd(Harness::v4(ClientAuth::Unsigned), "1.1");
    h.market_order(a, EURUSD, Side::Buy, "4500000", "1.1");
    (0..3).for_each(|_| {
        h.market_order(a, EURUSD, Side::Buy, DUST, "1.1");
    });
    // Before `V5` each dust buy truncates the long's entry one unit of precision lower.
    assert_eq!(entry(&h, a), f("1.099999997"));

    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V5 });
    // From `V5` each exact average sits just above a whole unit and rounds up onto the next one.
    (0..3).for_each(|_| {
        h.market_order(a, EURUSD, Side::Buy, DUST, "1.1");
    });
    assert_eq!(entry(&h, a), f("1.1"));
    let close = fill(h.market_order(a, EURUSD, Side::Sell, "4500000.000006", "1.1"));
    assert_eq!(close.realised_pnl, Fixed::ZERO);
}

#[test]
fn a_financing_charge_rounds_up_and_a_credit_toward_zero() {
    let mut h = Harness::v5();
    // Longs pay 34% + 2.5% a year, 0.1% a day; shorts receive 34% - 2.5%.
    h.admin(AdminCommand::SetBenchmarkRate { benchmark: "SOFR".into(), annual_bps: 3_400 });
    let (long, short, dust, dust_short) = (acct(1), acct(2), acct(3), acct(4));
    for account in [long, short, dust, dust_short] {
        h.deposit(account, "10000");
    }
    h.price(BTC, "100000.123456789");
    h.market_order(long, BTC, Side::Buy, "0.333333333", "100000.123456789");
    h.market_order(short, BTC, Side::Sell, "0.333333333", "100000.123456789");
    h.price(ETH, "1000.000000001");
    h.market_order(dust, ETH, Side::Buy, DUST, "1000.000000001");
    h.price(AAPL, "1158.5");
    h.market_order(dust_short, AAPL, Side::Sell, DUST, "1158.5");

    // Past the 22:00 London rollover. On 33,333.374452263 the long owes 33.3333744522…, and the
    // short is owed 28.7671587738…. The dust long's notional is 1000.000000001 units of
    // precision, so its charge, 1.000000000001 units, rounds up to 2 only if the notional rounds up.
    // The dust short's notional is 1158.5 units: rounded down to 1158 it earns 0.997 of a unit a
    // day, which rounds toward zero to nothing, where 1159 would earn a whole unit.
    h.advance_to(ny(2026, 10, 20, 17, 1));
    let charges: Vec<(AccountId, Fixed)> = h
        .cmd(Command::Tick)
        .into_iter()
        .filter_map(|event| match event {
            Event::Financing { account, amount, .. } => Some((account, amount)),
            _ => None,
        })
        .collect();
    assert_eq!(charges, [(long, f("33.333374453")), (short, f("-28.767158773")), (dust, f("0.000000002"))]);
}

#[test]
fn financing_on_a_dust_priced_long_marked_at_the_input_ceiling_stays_in_range() {
    let (mut h, a) = dust_priced_long(&["1000000000"; 100]);
    // 100bn units at 1bn is 1e20 of notional, charged 200% a year.
    h.advance_to(ny(2026, 10, 20, 17, 1));
    let charges: Vec<Fixed> = h
        .cmd(Command::Tick)
        .into_iter()
        .filter_map(|event| match event {
            Event::Financing { account, amount, .. } if account == a => Some(amount),
            _ => None,
        })
        .collect();
    assert_eq!(charges, [f("547945205479452054.794520548")]);
}

#[test]
fn adding_to_a_dust_priced_long_marked_at_the_input_ceiling_stays_in_range() {
    // 170.1bn units: the largest long whose notional at the input ceiling the engine can still
    // value, and enough that weighting the position rather than the fill would overflow.
    let mut lots = vec!["1000000000"; 170];
    lots.push("100000000");
    let (mut h, a) = dust_priced_long(&lots);
    let events = h.market_order(a, DUSTY, Side::Buy, DUST, "1000000000");
    assert_eq!(rejection(&events), &Reject::HouseCapacity);
    assert_eq!(entry(&h, a), f(DUST));
}

// ── Helpers ──

/// `h` with `EURUSD` listed and priced at `mid`, and account 1 funded with 1m.
fn eurusd(mut h: Harness, mid: &str) -> (Harness, AccountId) {
    let spec = MarketSpec {
        spread_tiers: vec![Tier { up_to: None, bps: 0 }],
        ..market(EURUSD, "EUR-USD", AssetClass::Fx, Schedule::AlwaysOpen, 100)
    };
    h.admin(AdminCommand::UpsertMarket(spec));
    h.price(EURUSD, mid);
    let a = acct(1);
    h.deposit(a, "1000000");
    (h, a)
}

/// `DUSTY` listed with the most financing a listing allows, account 1 long `lots` bought at one
/// unit of precision each, then the market marked at `Fixed::MAX_INPUT`.
fn dust_priced_long(lots: &[&str]) -> (Harness, AccountId) {
    let mut h = Harness::v5();
    h.admin(AdminCommand::SetBenchmarkRate { benchmark: "SOFR".into(), annual_bps: 10_000 });
    let mut spec = market(DUSTY, "DUSTY", AssetClass::Crypto, Schedule::AlwaysOpen, 1_000);
    spec.financing.markup_bps = 10_000;
    h.admin(AdminCommand::UpsertMarket(spec));
    h.price(DUSTY, DUST);
    let a = acct(1);
    h.deposit(a, "1000");
    for lot in lots {
        assert_eq!(fills(&h.market_order(a, DUSTY, Side::Buy, lot, DUST)).len(), 1, "lot {lot}");
    }
    h.price(DUSTY, "1000000000");
    (h, a)
}

/// The one fill among `events`; panics if there is not exactly one.
fn fill(events: Vec<Event>) -> Fill {
    match &fills(&events)[..] {
        [fill] => (*fill).clone(),
        _ => panic!("expected one fill, got {events:?}"),
    }
}

/// The averaged entry of the account's open position, in whichever market it holds one.
fn entry(h: &Harness, account: AccountId) -> Fixed {
    let positions = &h.engine.accounts()[&account].positions;
    assert_eq!(positions.len(), 1, "{positions:?}");
    positions.values().next().expect("one position").entry
}
