//! Time: venue hours with gap-aware fills at the open, calendar updates, and the daily financing
//! rollover, end to end through `Engine::apply`.

mod common;

use chrono::{TimeZone, Utc};
use common::{AAPL, BTC, Harness, acct, f, fills, market, ny, rejection, us_equities};
use engine_core::command::{AdminCommand, Command, OrderKind, Sequenced};
use engine_core::event::{Event, Reject};
use engine_core::session::Schedule;
use engine_core::types::{AssetClass, Side};
use engine_core::{Engine, Fixed};

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

#[test]
fn a_calendar_update_applies_from_its_seq() {
    let mut h = Harness::new();
    // Thu 22 Oct 2026 14:00: the listed calendar keeps AAPL open all afternoon.
    h.advance_to(ny(2026, 10, 22, 14, 0));
    assert!(h.cmd(Command::Tick).is_empty());
    assert!(h.engine.markets()[&AAPL].open);

    // An update closing at 13:00 today and tomorrow shuts the market at its own seq.
    let events = h.cmd(calendar_update(&["2026-10-22", "2026-10-23"]));
    assert_eq!(events, [Event::MarketClosed { market: AAPL }, Event::AdminApplied]);
    assert_eq!(h.engine.markets()[&AAPL].spec.schedule, early_closes(&["2026-10-22", "2026-10-23"]));
    // Friday follows the new calendar: open from 09:30, shut from 13:00.
    h.advance_to(ny(2026, 10, 23, 9, 30));
    assert_eq!(h.cmd(Command::Tick), [Event::MarketOpened { market: AAPL }]);
    h.advance_to(ny(2026, 10, 23, 13, 0));
    assert_eq!(h.cmd(Command::Tick), [Event::MarketClosed { market: AAPL }]);
}

#[test]
fn a_calendar_update_that_changes_nothing_is_refused_and_the_calendar_kept() {
    let mut h = Harness::new();
    // A 13:00 close typed for Saturday 24 Oct, when the venue is already shut.
    let events = h.cmd(calendar_update(&["2026-10-24"]));
    assert!(matches!(&events[..], [Event::AdminRejected { reason }] if reason.contains("already shut")), "{events:?}");
    assert_eq!(h.engine.markets()[&AAPL].spec.schedule, us_equities());
}

#[test]
fn a_log_with_a_calendar_update_replays_to_the_roots_it_recorded() {
    let mut h = Harness::new();
    let mut recorded = roots(&h.log);
    let mut step = |h: &mut Harness, cmd| {
        h.cmd(cmd);
        recorded.push(h.engine.state_root());
    };
    h.advance_to(ny(2026, 10, 22, 14, 0));
    step(&mut h, Command::Tick);
    step(&mut h, calendar_update(&["2026-10-22", "2026-10-23"]));
    h.advance_to(ny(2026, 10, 23, 13, 0));
    step(&mut h, Command::Tick);

    // A verifier replaying the log from genesis reaches every root the live engine signed, before
    // the update and after it.
    assert_eq!(roots(&h.log), recorded);
}

// ── Helpers ──

/// An admin command re-listing AAPL with a 13:00 New York close on each date.
fn calendar_update(dates: &[&str]) -> Command {
    let spec = market(AAPL, "AAPL", AssetClass::Equity, early_closes(dates), 2_000);
    Command::Admin(AdminCommand::UpsertMarket(spec))
}

/// US equity hours with a 13:00 close on each date.
fn early_closes(dates: &[&str]) -> Schedule {
    let mut schedule = serde_json::to_value(us_equities()).unwrap();
    let closures: Vec<_> = dates
        .iter()
        .map(|d| serde_json::json!({ "from": format!("{d}T13:00:00"), "to": format!("{d}T16:00:00") }))
        .collect();
    schedule["closures"] = closures.into();
    serde_json::from_value(schedule).unwrap()
}

/// The state root after each command of `log`, read back from its JSON encoding as a verifier reads
/// the published log and replayed on a fresh engine.
fn roots(log: &[Sequenced]) -> Vec<[u8; 32]> {
    let mut engine = Engine::new();
    log.iter()
        .map(|sequenced| {
            let published: Sequenced = serde_json::from_str(&serde_json::to_string(sequenced).unwrap()).unwrap();
            engine.apply(&published).expect("in sequence");
            engine.state_root()
        })
        .collect()
}
