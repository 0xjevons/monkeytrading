//! Stress levels from `V13`: re-evaluated before every fill and after every command, fast moves
//! measured over a trailing window, and divergence triggers as a share of each market's bound, end
//! to end through `Engine::apply`.

mod common;

use std::cell::RefCell;

use common::events::fills;
use common::stress::{
    BOUNDED, at_a_million_nav, bounded, diverge, divergence_only, divergence_params, halted_us_equities,
    with_divergence_shares,
};
use common::{AAPL, BTC, DE40, ETH, EURUSD, Harness, UNLISTED, acct, de40, f, market, ny, params};
use engine_core::Fixed;
use engine_core::command::{AdminCommand, Command, OrderKind};
use engine_core::event::Event;
use engine_core::market::MarketSpec;
use engine_core::params::{ProtocolParams, StressLevel, StressTrigger};
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::types::{AssetClass, Millis, Side};
use proptest::collection::vec;
use proptest::prop_oneof;
use proptest::sample::select;
use proptest::test_runner::{Config, RngSeed, TestCaseError, TestRunner};

/// BTC raised to the harness's one stress level, which charges 5bps on every fill.
const RAISED: Event = Event::StressChanged { market: BTC, level: 1 };
/// The harness move window, and the bucket width the engine keeps it in.
const WINDOW_MS: Millis = 60_000;
const BUCKET_MS: Millis = WINDOW_MS / 10;

#[test]
fn a_fill_that_crosses_the_utilisation_trigger_stresses_the_next_fill() {
    // NAV of 1m, a 50% BTC cap, and the 5bps level at 80% use.
    for (version, mut h) in [(ProtocolVersion::V13, Harness::v13()), (ProtocolVersion::V12, Harness::v12())] {
        let fixed = version >= ProtocolVersion::V13;
        let a = acct(1);
        at_a_million_nav(&mut h);
        h.deposit(a, "100000");
        h.price(BTC, "100000");
        // 410k of a 500k cap is 82%: this fill pays the mid, and from V13 raises the level.
        let ev = h.market_order(a, BTC, Side::Buy, "4.1", "100000");
        assert_eq!(fills(&ev)[0].spread_bps, 0, "{version:?}");
        assert_eq!(ev.contains(&RAISED), fixed, "{version:?}: {ev:?}");
        // The next buy, before any new price, pays the stress spread from V13.
        let ev = h.market_order(a, BTC, Side::Buy, "0.1", "100000");
        assert_eq!(fills(&ev)[0].spread_bps, if fixed { 5 } else { 0 }, "{version:?}");
    }
}

#[test]
fn a_fill_that_crosses_the_utilisation_trigger_stresses_the_next_fill_in_the_same_command() {
    for (version, mut h) in [(ProtocolVersion::V13, Harness::v13()), (ProtocolVersion::V12, Harness::v12())] {
        let (a, b) = (acct(1), acct(2));
        at_a_million_nav(&mut h);
        h.deposit(a, "100000");
        h.deposit(b, "100000");
        h.price(BTC, "99000");
        // Two buy stops that one price triggers: the first takes the house to 82% of its cap.
        h.order(a, BTC, Side::Buy, "4.1", OrderKind::Stop { price: f("100000") });
        h.order(b, BTC, Side::Buy, "0.1", OrderKind::Stop { price: f("100000") });
        let ev = h.price(BTC, "100000");
        let spreads: Vec<i64> = fills(&ev).iter().map(|fill| fill.spread_bps).collect();
        let expected = if version >= ProtocolVersion::V13 { [0, 5] } else { [0, 0] };
        assert_eq!(spreads, expected, "{version:?}");
    }
}

#[test]
fn every_command_that_moves_utilisation_re_evaluates_the_level() {
    type Change = fn(&mut Harness) -> Vec<Event>;
    // From 78% of the BTC cap, each command takes the house to 80% or more without a BTC price or
    // fill: NAV falls 3% or more, the trigger falls to 70%, or the cap to 48% of NAV.
    let changes: [(&str, Change); 4] = [
        ("house capital out", |h| h.cmd(Command::HouseCapital { amount: f("-30000"), chain_ref: "out".into() })),
        ("a trader's gain in another market", |h| h.price(ETH, "3300")),
        ("a lower utilisation trigger", |h| {
            let mut lower = params();
            lower.stress_levels[1].trigger.as_mut().expect("level 1 has a trigger").utilisation_bps = Some(7_000);
            let lower =
                if h.engine.protocol_version() >= ProtocolVersion::V13 { with_divergence_shares(lower) } else { lower };
            h.operate(AdminCommand::SetParams(lower))
        }),
        ("a lower house cap", |h| {
            let btc = market(BTC, "BTC-USD", AssetClass::Crypto, Schedule::AlwaysOpen, 1_000);
            h.operate(AdminCommand::UpsertMarket(MarketSpec { house_cap_bps: 4_800, ..btc }))
        }),
    ];
    for (name, change) in changes {
        for (version, mut h) in [(ProtocolVersion::V13, Harness::v13()), (ProtocolVersion::V12, Harness::v12())] {
            let fixed = version >= ProtocolVersion::V13;
            let (a, b, c) = (acct(1), acct(2), acct(3));
            at_a_million_nav(&mut h);
            for account in [a, b, c] {
                h.deposit(account, "100000");
            }
            h.price(BTC, "100000");
            h.price(ETH, "3000");
            h.market_order(a, BTC, Side::Buy, "3.9", "100000");
            h.market_order(b, ETH, Side::Buy, "100", "3000");
            assert_eq!(h.engine.markets()[&BTC].stress.level, 0, "{name}, {version:?}");

            let ev = change(&mut h);
            assert_eq!(ev.contains(&RAISED), fixed, "{name}, {version:?}: {ev:?}");
            let ev = h.market_order(c, BTC, Side::Buy, "0.01", "100000");
            assert_eq!(fills(&ev)[0].spread_bps, if fixed { 5 } else { 0 }, "{name}, {version:?}");
        }
    }
}

#[test]
fn time_alone_re_evaluates_the_level() {
    // Calm quotes inside the cooldown hold the level, and the tick once it has passed steps it down
    // from V13; before it, a tick that moves no index leaves the level raised.
    for (version, mut h) in [(ProtocolVersion::V13, Harness::v13()), (ProtocolVersion::V12, Harness::v12())] {
        h.quotes(BTC, &["100000", "100150"]);
        h.advance(299_000);
        h.price(BTC, "100000");
        assert_eq!(h.engine.markets()[&BTC].stress.level, 1, "{version:?}");
        h.advance(1_000);
        let ev = h.cmd(Command::Tick);
        let stepped_down = ev.contains(&Event::StressChanged { market: BTC, level: 0 });
        assert_eq!(stepped_down, version >= ProtocolVersion::V13, "{version:?}: {ev:?}");
    }
}

#[test]
fn utilisation_values_exposure_in_usd_at_the_fx_index() {
    // NAV of 1m and a 50% Germany 40 cap: EUR 399,800 is 79.96% of it at an EUR-USD index of 1.0, and
    // 80.04% at 1.001, so EUR-USD alone moving to 1.001 raises Germany 40, and its next fill pays the
    // 5bps level.
    let mut h = Harness::v13();
    at_a_million_nav(&mut h);
    let eurusd = market(EURUSD, "EUR-USD", AssetClass::Fx, Schedule::AlwaysOpen, 200);
    h.operator_admin(AdminCommand::UpsertMarket(eurusd));
    h.operator_admin(AdminCommand::UpsertMarket(de40(500)));
    let (a, b) = (acct(1), acct(2));
    h.deposit(a, "100000");
    h.deposit(b, "100000");
    h.price(EURUSD, "1");
    h.price(DE40, "20000");
    h.market_order(a, DE40, Side::Buy, "19.99", "20000");
    assert_eq!(h.engine.markets()[&DE40].stress.level, 0);
    let ev = h.price(EURUSD, "1.001");
    assert!(ev.contains(&Event::StressChanged { market: DE40, level: 1 }), "{ev:?}");
    let ev = h.market_order(b, DE40, Side::Buy, "0.01", "20000");
    assert_eq!(fills(&ev)[0].spread_bps, 5, "{ev:?}");
}

#[test]
fn an_admin_command_s_outcome_stays_its_last_event() {
    // BTC's cooldown ends as a refused command arrives: its step down belongs to that command, and
    // comes before the refusal.
    let mut h = Harness::v13();
    h.quotes(BTC, &["100000", "100150"]);
    h.advance(299_500);
    h.price(BTC, "100000");
    h.advance(600);
    let ev = h.operate(AdminCommand::SetReduceOnly { market: Some(UNLISTED), reduce_only: true });
    assert!(ev.contains(&Event::StressChanged { market: BTC, level: 0 }), "{ev:?}");
    assert!(matches!(ev.last(), Some(Event::AdminRejected { .. })), "{ev:?}");
}

#[test]
fn a_jump_on_the_first_price_a_window_after_the_last_counts_as_a_move() {
    // 300bps against the 200bps trigger, on the first price a whole window after the last.
    for (version, mut h) in [(ProtocolVersion::V13, Harness::v13()), (ProtocolVersion::V12, Harness::v12())] {
        h.price(BTC, "100000");
        h.advance(WINDOW_MS);
        let ev = h.price(BTC, "97000");
        assert_eq!(ev.contains(&RAISED), version >= ProtocolVersion::V13, "{version:?}: {ev:?}");
    }
    // A millisecond later the earlier price has left the window.
    let mut h = Harness::v13();
    h.price(BTC, "100000");
    h.advance(WINDOW_MS + 1);
    let ev = h.price(BTC, "97000");
    assert!(!ev.contains(&RAISED), "{ev:?}");
}

#[test]
fn a_move_that_straddles_two_windows_counts() {
    for (version, mut h) in [(ProtocolVersion::V13, Harness::v13()), (ProtocolVersion::V12, Harness::v12())] {
        h.price(BTC, "100000");
        h.advance(50_000);
        h.price(BTC, "100000");
        // 70s after the first price and 20s after the second: before V13 the window restarted at 60s.
        h.advance(20_000);
        let ev = h.price(BTC, "97000");
        assert_eq!(ev.contains(&RAISED), version >= ProtocolVersion::V13, "{version:?}: {ev:?}");
    }
}

#[test]
fn a_move_within_any_trailing_window_raises_the_level_and_older_moves_do_not() {
    // Prices at random gaps, some on and either side of the window's edges. With no cooldown the
    // level after each price is the move reading's: raised by any move of 200bps from a price
    // within the window, and by none from a price a whole bucket older than it.
    let gap = prop_oneof![
        3 => 0..=2 * WINDOW_MS,
        1 => select(vec![0, 1, BUCKET_MS - 1, BUCKET_MS, WINDOW_MS - 1, WINDOW_MS, WINDOW_MS + 1]),
    ];
    let path = vec((gap, 9_000i64..=11_000), 1..=40);
    let config =
        Config { cases: 256, rng_seed: RngSeed::Fixed(0x5eed_0082), failure_persistence: None, ..Config::default() };
    // Prices that left the level at 0 and at 1, so the check cannot pass vacuously.
    let reached = RefCell::new([0usize; 2]);
    let result = TestRunner::new(config).run(&path, |path| {
        let mut h = Harness::v13();
        h.operator_admin(AdminCommand::SetParams(with_divergence_shares(ProtocolParams {
            stress_cooldown_ms: 0,
            ..params()
        })));
        let mut seen: Vec<(Millis, Fixed)> = Vec::new();
        for (gap_ms, mid) in path {
            h.advance(gap_ms);
            let mid = Fixed::from_int(mid);
            h.price(BTC, &mid.to_string());
            seen.push((h.ts, mid));
            let moved_within = |span_ms: Millis| {
                seen.iter().any(|(ts, earlier)| h.ts - ts <= span_ms && (mid - *earlier).abs().bps_of(*earlier) >= 200)
            };
            let level = h.engine.markets()[&BTC].stress.level;
            if moved_within(WINDOW_MS) && level != 1 {
                return Err(TestCaseError::fail(format!("a move within the window left level {level}: {seen:?}")));
            }
            if level == 1 && !moved_within(WINDOW_MS + BUCKET_MS - 1) {
                return Err(TestCaseError::fail(format!("no move within a window and a bucket raised it: {seen:?}")));
            }
            reached.borrow_mut()[level] += 1;
        }
        Ok(())
    });
    if let Err(failure) = result {
        panic!("{failure}");
    }
    let [calm, raised] = reached.into_inner();
    assert!(calm > 1_000 && raised > 1_000, "calm {calm}, raised {raised}");
}

#[test]
fn every_divergence_share_up_to_the_bound_is_reachable_in_every_market() {
    for bound in [1, 5, 10, 25, 50, 10_000] {
        for share in [0, 1, 2_500, 6_000, 8_000, 10_000] {
            let mut h = divergence_only(share, bound);
            // The least divergence whose share of the bound meets the trigger.
            let threshold = (share * bound + 9_999) / 10_000;
            if threshold > 0 {
                diverge(&mut h, threshold - 1);
                assert_eq!(h.engine.markets()[&BOUNDED].stress.level, 0, "bound {bound}, share {share}");
            }
            diverge(&mut h, threshold);
            assert_eq!(h.engine.markets()[&BOUNDED].stress.level, 1, "bound {bound}, share {share}");
            diverge(&mut h, bound);
            assert_eq!(h.engine.markets()[&BOUNDED].stress.level, 1, "bound {bound}, share {share}");
        }
    }
}

#[test]
fn launch_divergence_levels_scale_with_each_market_s_bound() {
    // Levels 1 and 2 at 60% and 80% of the bound, so crypto's 25bps bound keeps the 15 and 20bps
    // thresholds and every tighter bound reaches both.
    for (bound, level_1, level_2) in [(5, 3, 4), (10, 6, 8), (25, 15, 20)] {
        let mut h = Harness::v13();
        let trigger = |share| StressTrigger { divergence_share_bps: Some(share), ..StressTrigger::default() };
        let mut launch = ProtocolParams { stress_cooldown_ms: 0, ..with_divergence_shares(params()) };
        launch.stress_levels = vec![
            launch.stress_levels[0].clone(),
            StressLevel { size_spread_multiplier_bps: 20_000, min_spread_bps: 2, trigger: Some(trigger(6_000)) },
            StressLevel { size_spread_multiplier_bps: 40_000, min_spread_bps: 10, trigger: Some(trigger(8_000)) },
        ];
        h.operator_admin(AdminCommand::SetParams(launch));
        h.operator_admin(AdminCommand::UpsertMarket(bounded(bound)));
        let level_at = |h: &mut Harness, bps| {
            diverge(h, bps);
            h.engine.markets()[&BOUNDED].stress.level
        };
        let levels: Vec<usize> =
            [level_1 - 1, level_1, level_2 - 1, level_2, bound].iter().map(|bps| level_at(&mut h, *bps)).collect();
        assert_eq!(levels, [0, 1, 1, 2, 2], "bound {bound}");
    }
}

#[test]
fn a_divergence_share_above_zero_and_a_zero_bound_market_are_refused_together() {
    // A market that tolerates no divergence prices only while its venues agree, so its share is
    // always zero: a share above it could never fire there, whichever command comes second.
    let refused = |ev: &[Event]| matches!(ev, [Event::AdminRejected { reason }, ..] if reason.contains("never fire"));
    let mut h = Harness::v13();
    let ev = h.operate(AdminCommand::UpsertMarket(bounded(0)));
    assert!(refused(&ev), "{ev:?}");
    let mut h = divergence_only(0, 0);
    let ev = h.operate(AdminCommand::SetParams(divergence_params(1)));
    assert!(refused(&ev), "{ev:?}");
    // A zero share fires at any divergence, so it may sit alongside a zero bound.
    diverge(&mut h, 0);
    assert_eq!(h.engine.markets()[&BOUNDED].stress.level, 1);
}

#[test]
fn a_move_across_a_halt_shorter_than_the_window_raises_the_level_from_v17() {
    // AAPL, halted 20s by a dated closure 10s after its last price. The 3% move across the halt
    // raises the level from V17, which keeps the move window across the reopening; before it the
    // open cleared the window, so the same move counts from nothing.
    for (version, mut h) in [(ProtocolVersion::V15, Harness::v15()), (ProtocolVersion::V17, Harness::v17())] {
        let schedule = halted_us_equities("2026-10-20T12:00:00", "2026-10-20T12:00:20");
        h.operator_admin(AdminCommand::UpsertMarket(market(AAPL, "AAPL", AssetClass::Equity, schedule, 2_000)));
        h.advance_to(ny(2026, 10, 20, 11, 59) + 50_000);
        h.price(AAPL, "230");
        h.advance_to(ny(2026, 10, 20, 12, 0));
        let ev = h.cmd(Command::Tick);
        assert!(ev.contains(&Event::MarketClosed { market: AAPL }), "{version:?}: {ev:?}");
        h.advance(20_000);
        // 30s after the last pre-halt price, inside the 60s window.
        let ev = h.price(AAPL, "223.1");
        let kept = version >= ProtocolVersion::V17;
        assert!(ev.contains(&Event::MarketOpened { market: AAPL }), "{version:?}: {ev:?}");
        assert_eq!(ev.contains(&Event::StressChanged { market: AAPL, level: 1 }), kept, "{version:?}: {ev:?}");
        // The pre-halt price survives the halt only under V17.
        let window = &h.engine.markets()[&AAPL].stress.move_window;
        assert_eq!(window.iter().any(|bucket| bucket.high == f("230")), kept, "{version:?}: {window:?}");
    }
}

#[test]
fn a_move_across_a_halt_longer_than_the_window_does_not_count() {
    // AAPL, halted 70s, its last price 80s before the reopening: the halt has outlasted the
    // window, so neither version counts the move, and V17's kept window holds no bucket older
    // than the window.
    for (version, mut h) in [(ProtocolVersion::V15, Harness::v15()), (ProtocolVersion::V17, Harness::v17())] {
        let schedule = halted_us_equities("2026-10-20T12:00:00", "2026-10-20T12:01:10");
        h.operator_admin(AdminCommand::UpsertMarket(market(AAPL, "AAPL", AssetClass::Equity, schedule, 2_000)));
        h.advance_to(ny(2026, 10, 20, 11, 59) + 50_000);
        h.price(AAPL, "230");
        h.advance_to(ny(2026, 10, 20, 12, 0));
        let ev = h.cmd(Command::Tick);
        assert!(ev.contains(&Event::MarketClosed { market: AAPL }), "{version:?}: {ev:?}");
        h.advance(70_000);
        let ev = h.price(AAPL, "223.1");
        assert!(ev.contains(&Event::MarketOpened { market: AAPL }), "{version:?}: {ev:?}");
        assert!(!ev.contains(&Event::StressChanged { market: AAPL, level: 1 }), "{version:?}: {ev:?}");
        let window = &h.engine.markets()[&AAPL].stress.move_window;
        assert!(window.iter().all(|bucket| h.ts - bucket.to <= WINDOW_MS), "{version:?}: {window:?}");
    }
}

#[test]
fn a_market_keeps_its_level_while_its_fx_pair_has_no_price() {
    // Germany 40, raised by a 3% move, keeps its level while EUR-USD has no price, in every
    // version that reads stress this way: with no FX index there is no reading, so nothing steps
    // the level down even past the cooldown, and the move window freezes with it. A fresh EUR-USD
    // index restores the reading; the window has aged past the pre-quiet move, so the level steps
    // down at once.
    for (version, mut h) in [(ProtocolVersion::V15, Harness::v15()), (ProtocolVersion::V17, Harness::v17())] {
        h.list_fx();
        h.price(EURUSD, "1.1");
        h.price(DE40, "20000");
        let ev = h.price(DE40, "19400");
        assert!(ev.contains(&Event::StressChanged { market: DE40, level: 1 }), "{version:?}: {ev:?}");
        // EUR-USD goes quiet: its 1s quote age passes. Germany 40, its own quotes fresh, still
        // gets no reading, and calm prices change nothing.
        h.advance(2_000);
        let ev = h.price(DE40, "19400");
        assert!(!ev.iter().any(|e| matches!(e, Event::StressChanged { .. })), "{version:?}: {ev:?}");
        assert_eq!(h.engine.markets()[&DE40].stress.level, 1);
        // Past the 300s cooldown the level still holds: with Germany 40's own quotes also stale,
        // the tick reads nothing, and there is no step down.
        h.advance(300_000);
        let ev = h.cmd(Command::Tick);
        assert!(!ev.iter().any(|e| matches!(e, Event::StressChanged { market: DE40, .. })), "{version:?}: {ev:?}");
        assert_eq!(h.engine.markets()[&DE40].stress.level, 1);
        // Germany 40's quotes are fresh again and EUR-USD's are still quiet: still no reading.
        let ev = h.price(DE40, "19400");
        assert!(!ev.iter().any(|e| matches!(e, Event::StressChanged { .. })), "{version:?}: {ev:?}");
        assert_eq!(h.engine.markets()[&DE40].stress.level, 1);
        // A fresh EUR-USD index restores the reading: the aged-out window leaves the move behind.
        let ev = h.price(EURUSD, "1.1");
        assert!(ev.contains(&Event::StressChanged { market: DE40, level: 0 }), "{version:?}: {ev:?}");
    }
}

#[test]
fn a_closed_or_unpriced_market_keeps_its_level() {
    // AAPL closes at 16:00 New York. Venues 19bps apart afterwards, over 20% of its 50bps bound,
    // leave it calm, since nothing fills there.
    let mut h = Harness::v13();
    h.advance_to(ny(2026, 10, 20, 16, 30));
    let ev = h.cmd(Command::Tick);
    assert!(ev.contains(&Event::MarketClosed { market: AAPL }), "{ev:?}");
    let ev = h.quotes(AAPL, &["230", "230.46"]);
    assert!(!ev.iter().any(|e| matches!(e, Event::StressChanged { .. })), "{ev:?}");
    assert_eq!(h.engine.markets()[&AAPL].stress.level, 0);

    // BTC, raised by divergence, keeps its level past the cooldown while its quotes are stale, and
    // steps down with the next calm price.
    h.quotes(BTC, &["100000", "100150"]);
    assert_eq!(h.engine.markets()[&BTC].stress.level, 1);
    h.advance(400_000);
    h.cmd(Command::Tick);
    assert_eq!(h.engine.markets()[&BTC].stress.level, 1);
    let ev = h.price(BTC, "100000");
    assert!(ev.contains(&Event::StressChanged { market: BTC, level: 0 }), "{ev:?}");
}

#[test]
fn the_move_window_stays_bounded_however_often_prices_arrive() {
    let mut h = Harness::v13();
    for _ in 0..1_000 {
        h.advance(250);
        h.price(BTC, "100000");
    }
    // A minute of prices every 250ms is 240 mids, kept as the range of each tenth of the window.
    let window = &h.engine.markets()[&BTC].stress.move_window;
    assert!(window.len() <= 12, "{}", window.len());
}
