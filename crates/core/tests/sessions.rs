//! Time: venue hours with gap-aware fills at the open, calendar updates, and the daily financing
//! rollover, end to end through `Engine::apply`.

mod common;

use chrono::{TimeZone, Utc};
use common::events::{fills, rejection};
use common::{AAPL, BTC, ETH, Harness, acct, f, market, ny, us_equities};
use engine_core::auth::ClientAuth;
use engine_core::command::{AdminCommand, Command, OrderKind, Sequenced};
use engine_core::event::{Event, Reject};
use engine_core::market::MarketSpec;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::types::{AssetClass, MarketId, Side};
use engine_core::{Engine, Fixed};

/// Listings financed at ESTR, ESTR, and SONIA, and one whose listing names "Sofr".
const EURO_A: MarketId = 20;
const EURO_B: MarketId = 21;
const STERLING: MarketId = 22;
const GOLD: MarketId = 30;

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
fn a_market_whose_benchmark_has_no_rate_accrues_no_financing_from_v13() {
    // Genesis sets SOFR, and a gold listing names it "Sofr".
    let mut h = Harness::v12();
    h.operator_admin(AdminCommand::SetBenchmarkRate { benchmark: "SOFR".into(), annual_bps: 500 });
    h.operator_admin(AdminCommand::UpsertMarket(listing(GOLD, "Sofr")));
    let (gold, bitcoin) = (acct(1), acct(2));
    h.deposit(gold, "50000");
    h.deposit(bitcoin, "50000");
    h.price(GOLD, "2000");
    h.price(BTC, "100000");
    h.market_order(gold, GOLD, Side::Buy, "1", "2000");
    h.market_order(bitcoin, BTC, Side::Buy, "1", "100000");

    // Before `V13` the missing rate counts as zero, so the gold long pays the 2.5% markup alone.
    let ev = rollover(&mut h, 20);
    assert!(ev.contains(&Event::Financing { account: gold, market: GOLD, amount: f("0.136986302") }), "{ev:?}");

    // From `V13` the gold long accrues nothing and the rollover records the benchmark, while the
    // bitcoin long still pays SOFR plus the markup.
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V13 });
    let balance = h.balance(gold);
    let ev = rollover(&mut h, 21);
    assert!(ev.contains(&missing_rate("Sofr", &[GOLD])), "{ev:?}");
    assert_eq!(financed(&ev), [BTC]);
    assert!(ev.contains(&Event::Financing { account: bitcoin, market: BTC, amount: f("20.547945206") }), "{ev:?}");
    assert_eq!(h.balance(gold), balance);
    h.assert_conserved(Fixed::ZERO);
}

#[test]
fn every_rollover_records_each_benchmark_with_no_rate_and_finances_only_rated_markets() {
    // BTC, ETH, and AAPL use SOFR, the two euro listings ESTR, and sterling SONIA. They are listed
    // before any benchmark has a rate, as the production genesis lists its markets, and then only
    // SONIA gets one.
    let mut h = Harness::v13();
    for (id, benchmark) in [(EURO_A, "ESTR"), (EURO_B, "ESTR"), (STERLING, "SONIA")] {
        h.operator_admin(AdminCommand::UpsertMarket(listing(id, benchmark)));
    }
    h.operator_admin(AdminCommand::SetBenchmarkRate { benchmark: "SONIA".into(), annual_bps: 400 });
    let a = acct(1);
    h.deposit(a, "1000000");
    for (market, mid) in [(BTC, "100000"), (EURO_A, "1.1"), (EURO_B, "24000"), (STERLING, "1.3")] {
        h.price(market, mid);
        assert_eq!(fills(&h.market_order(a, market, Side::Buy, "1", mid)).len(), 1);
    }
    let ev = rollover(&mut h, 20);
    let sofr = missing_rate("SOFR", &[BTC, ETH, AAPL]);
    assert_eq!(missing_rates(&ev), [missing_rate("ESTR", &[EURO_A, EURO_B]), sofr]);
    assert_eq!(financed(&ev), [STERLING]);

    // Once SOFR has a rate, only the ESTR listings go unfinanced, recorded at each rollover missed
    // during downtime.
    h.operator_admin(AdminCommand::SetBenchmarkRate { benchmark: "SOFR".into(), annual_bps: 500 });
    let ev = rollover(&mut h, 22);
    let estr = missing_rate("ESTR", &[EURO_A, EURO_B]);
    assert_eq!(missing_rates(&ev), [estr.clone(), estr]);
    assert_eq!(financed(&ev), [BTC, STERLING, BTC, STERLING]);

    // Once every benchmark has a rate, every position is financed and nothing is recorded.
    h.operator_admin(AdminCommand::SetBenchmarkRate { benchmark: "ESTR".into(), annual_bps: 300 });
    let ev = rollover(&mut h, 23);
    assert!(missing_rates(&ev).is_empty(), "{ev:?}");
    assert_eq!(financed(&ev), [BTC, EURO_A, EURO_B, STERLING]);
    h.assert_conserved(Fixed::ZERO);
}

#[test]
fn once_a_benchmark_has_a_rate_a_listing_names_only_one_with_a_rate_from_v13() {
    let refused = |events: Vec<Event>| {
        assert!(
            matches!(&events[..], [Event::AdminRejected { reason }] if reason == "benchmark Sofr has no rate"),
            "{events:?}"
        );
    };
    let mut h = Harness::v13();
    // With no rate yet, a listing may name any benchmark.
    h.operator_admin(AdminCommand::UpsertMarket(listing(GOLD, "Sofr")));
    h.operator_admin(AdminCommand::SetBenchmarkRate { benchmark: "SOFR".into(), annual_bps: 500 });
    // Once SOFR has a rate, a new listing naming "Sofr" is refused, and so is moving BTC onto it.
    refused(h.operate(AdminCommand::UpsertMarket(listing(EURO_A, "Sofr"))));
    let btc = market(BTC, "BTC-USD", AssetClass::Crypto, Schedule::AlwaysOpen, 1_000);
    let moved = MarketSpec { financing: listing(BTC, "Sofr").financing, ..btc.clone() };
    refused(h.operate(AdminCommand::UpsertMarket(moved)));
    assert_eq!(h.engine.markets()[&BTC].spec, btc);
    // An update that keeps gold's benchmark applies, as does a listing naming one with a rate.
    h.operator_admin(AdminCommand::UpsertMarket(MarketSpec { max_slippage_bps: 30, ..listing(GOLD, "Sofr") }));
    h.operator_admin(AdminCommand::UpsertMarket(listing(EURO_A, "SOFR")));

    // Before `V13` the same listing applies.
    let mut h = Harness::v12();
    h.operator_admin(AdminCommand::SetBenchmarkRate { benchmark: "SOFR".into(), annual_bps: 500 });
    h.operator_admin(AdminCommand::UpsertMarket(listing(EURO_A, "Sofr")));
}

#[test]
fn a_calendar_update_applies_from_its_seq() {
    let mut h = v4();
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
    let mut h = v4();
    // A 13:00 close typed for Saturday 24 Oct, when the venue is already shut.
    let events = h.cmd(calendar_update(&["2026-10-24"]));
    assert!(matches!(&events[..], [Event::AdminRejected { reason }] if reason.contains("already shut")), "{events:?}");
    assert_eq!(h.engine.markets()[&AAPL].spec.schedule, us_equities());
}

#[test]
fn a_log_with_a_calendar_update_replays_to_the_roots_it_recorded() {
    let mut h = v4();
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

#[test]
fn dated_exceptions_are_accepted_only_from_v4() {
    // Under each version, a weekly schedule without exceptions is accepted, and one with an early
    // close, or with a special session only, is accepted from V4 on.
    let updates = [("early close", calendar_update(&["2026-10-22"])), ("special session", special_session_update())];
    for (kind, update) in updates {
        let cases = [
            (ProtocolVersion::V2, Harness::new()),
            (ProtocolVersion::V3, Harness::v3(ClientAuth::Unsigned)),
            (ProtocolVersion::V4, v4()),
        ];
        for (version, mut h) in cases {
            assert_eq!(h.engine.protocol_version(), version);
            let plain = Command::Admin(AdminCommand::UpsertMarket(market(
                AAPL,
                "AAPL",
                AssetClass::Equity,
                us_equities(),
                2_000,
            )));
            assert_eq!(h.cmd(plain), [Event::AdminApplied], "{kind} {version:?}");
            let events = h.cmd(update.clone());
            if version >= ProtocolVersion::V4 {
                assert!(events.contains(&Event::AdminApplied), "{kind} {version:?}: {events:?}");
                assert!(h.engine.markets()[&AAPL].spec.schedule.has_exceptions(), "{kind}");
            } else {
                let refused =
                    matches!(&events[..], [Event::AdminRejected { reason }] if reason.contains("need protocol V4"));
                assert!(refused, "{kind} {version:?}: {events:?}");
                assert_eq!(h.engine.markets()[&AAPL].spec.schedule, us_equities(), "{kind}");
            }
        }
    }
}

// ── Helpers ──

/// `Harness::v4` with unsigned client requests.
fn v4() -> Harness {
    Harness::v4(ClientAuth::Unsigned)
}

/// A 24/7 listing financed at `benchmark` plus the harness markup.
fn listing(id: MarketId, benchmark: &str) -> MarketSpec {
    let mut spec = market(id, "GEN", AssetClass::Commodity, Schedule::AlwaysOpen, 1_000);
    spec.financing.benchmark = benchmark.into();
    spec
}

/// Ticks one second after the 22:00 London rollover on `day` October 2026, 21:00 UTC until the
/// clocks go back on the 25th.
fn rollover(h: &mut Harness, day: u32) -> Vec<Event> {
    assert!(day < 25, "London keeps BST until 25 Oct");
    h.advance_to(Utc.with_ymd_and_hms(2026, 10, day, 21, 0, 1).unwrap().timestamp_millis());
    h.cmd(Command::Tick)
}

/// The record of `benchmark` having no rate for `markets`.
fn missing_rate(benchmark: &str, markets: &[MarketId]) -> Event {
    Event::BenchmarkRateMissing { benchmark: benchmark.into(), markets: markets.to_vec() }
}

/// The missing-rate records among `events`.
fn missing_rates(events: &[Event]) -> Vec<Event> {
    events.iter().filter(|e| matches!(e, Event::BenchmarkRateMissing { .. })).cloned().collect()
}

/// The market of each financing charge among `events`, in order.
fn financed(events: &[Event]) -> Vec<MarketId> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Financing { market, .. } => Some(*market),
            _ => None,
        })
        .collect()
}

/// An admin command re-listing AAPL with a 13:00 New York close on each date.
fn calendar_update(dates: &[&str]) -> Command {
    let spec = market(AAPL, "AAPL", AssetClass::Equity, early_closes(dates), 2_000);
    Command::Admin(AdminCommand::UpsertMarket(spec))
}

/// An admin command re-listing AAPL with a Saturday 24 Oct 2026 10:00-12:00 special session only.
fn special_session_update() -> Command {
    let mut schedule = serde_json::to_value(us_equities()).unwrap();
    schedule["special_sessions"] = serde_json::json!([{ "from": "2026-10-24T10:00:00", "to": "2026-10-24T12:00:00" }]);
    let spec = market(AAPL, "AAPL", AssetClass::Equity, serde_json::from_value(schedule).unwrap(), 2_000);
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
