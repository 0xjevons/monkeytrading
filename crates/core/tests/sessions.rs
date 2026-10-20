//! Time: venue hours with gap-aware fills at the open, and the daily financing rollover, end to
//! end through `Engine::apply`.

mod common;

use chrono::{TimeZone, Utc};
use common::{AAPL, BTC, Harness, acct, f, fills, ny, rejection};
use engine_core::Fixed;
use engine_core::command::{AdminCommand, Command, OrderKind};
use engine_core::event::{Event, Reject};
use engine_core::types::Side;

#[test]
fn closed_markets_do_not_trade_and_stops_fill_at_the_gapped_open() {
    let mut h = Harness::new();
    let a = acct(1);
    h.deposit(a, "100000");
    h.price(AAPL, "200");
    let ev = h.market_order(a, AAPL, Side::Buy, "10", "200");
    assert_eq!(fills(&ev).len(), 1);
    let ev = h.order(a, AAPL, Side::Sell, "10", OrderKind::Stop { price: f("190") });
    assert!(matches!(ev[0], Event::OrderAccepted { .. }));

    // Tue 16:30 NY: closed.
    h.advance_to(ny(2026, 10, 20, 16, 30));
    let ev = h.cmd(Command::Tick);
    assert!(ev.contains(&Event::MarketClosed { market: AAPL }));
    let ev = h.market_order(a, AAPL, Side::Buy, "1", "200");
    assert_eq!(rejection(&ev), &Reject::MarketClosed);
    // After-hours prints never trigger anything.
    assert!(fills(&h.price(AAPL, "180")).is_empty());

    // Wed open gaps down through the stop: fills at the open, not at 190.
    h.advance_to(ny(2026, 10, 21, 9, 30));
    let ev = h.price(AAPL, "170");
    assert!(ev.contains(&Event::MarketOpened { market: AAPL }));
    let fill = fills(&ev)[0];
    assert_eq!((fill.price, fill.realised_pnl), (f("170"), f("-300")));
    assert!(h.engine.accounts()[&a].orders.is_empty());
}

#[test]
fn limit_orders_placed_while_closed_fill_at_a_better_open() {
    let mut h = Harness::new();
    let a = acct(1);
    h.deposit(a, "100000");
    h.price(AAPL, "200");
    h.advance_to(ny(2026, 10, 20, 17, 0));
    // A buy limit at 195 may rest while the market is shut.
    let ev = h.order(a, AAPL, Side::Buy, "10", OrderKind::Limit { price: f("195") });
    assert!(matches!(ev[..], [Event::MarketClosed { .. }, Event::OrderAccepted { .. }]), "{ev:?}");
    // The open gaps below the limit: the order fills at the open price, better than 195.
    h.advance_to(ny(2026, 10, 21, 9, 30));
    let ev = h.price(AAPL, "190");
    assert_eq!(fills(&ev)[0].price, f("190"));
}

#[test]
fn overnight_financing_at_rollover() {
    let mut h = Harness::new();
    let (long, short) = (acct(1), acct(2));
    h.admin(AdminCommand::SetBenchmarkRate { benchmark: "SOFR".into(), annual_bps: 500 });
    h.deposit(long, "50000");
    h.deposit(short, "50000");
    h.price(BTC, "100000");
    h.market_order(long, BTC, Side::Buy, "1", "100000");
    h.market_order(short, BTC, Side::Sell, "1", "100000");

    // 22:00 London on 20 Oct 2026 is 21:00 UTC (BST).
    h.advance_to(Utc.with_ymd_and_hms(2026, 10, 20, 21, 0, 1).unwrap().timestamp_millis());
    let ev = h.cmd(Command::Tick);
    // Long pays (5% + 2.5%) / 365; short receives (5% - 2.5%) / 365.
    assert!(ev.contains(&Event::Financing { account: long, market: BTC, amount: f("20.547945205") }));
    assert!(ev.contains(&Event::Financing { account: short, market: BTC, amount: f("-6.849315068") }));
    // Only once per day.
    h.advance(60_000);
    assert!(!h.cmd(Command::Tick).iter().any(|e| matches!(e, Event::Financing { .. })));
    h.assert_conserved(Fixed::ZERO);
}

#[test]
fn financing_after_downtime_charges_every_missed_day() {
    let mut h = Harness::new();
    let long = acct(1);
    h.deposit(long, "50000");
    h.price(BTC, "100000");
    h.market_order(long, BTC, Side::Buy, "1", "100000");
    // Three rollovers pass (20, 21, 22 Oct) before the next command arrives.
    h.advance_to(Utc.with_ymd_and_hms(2026, 10, 22, 23, 0, 0).unwrap().timestamp_millis());
    let charges = h.cmd(Command::Tick).iter().filter(|e| matches!(e, Event::Financing { .. })).count();
    assert_eq!(charges, 3);
    h.assert_conserved(Fixed::ZERO);
}
