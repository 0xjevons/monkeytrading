//! Pricing: fills at the index mid, size spreads, bounded slippage, feed guards, toxicity, and
//! stress, end to end through `Engine::apply`.

mod common;

use common::{BTC, Harness, acct, f, fills, rejection};
use engine_core::Fixed;
use engine_core::command::{AdminCommand, Command, OrderKind};
use engine_core::event::{Event, Reject};
use engine_core::price::{MAX_QUOTE_LEAD_MS, SourceQuote};
use engine_core::types::{Millis, Side};

#[test]
fn normal_size_fills_at_mid_and_only_winners_pay() {
    let mut h = Harness::new();
    let a = acct(1);
    h.deposit(a, "10000");
    h.price(BTC, "100000");

    let ev = h.market_order(a, BTC, Side::Buy, "0.1", "100000");
    let fill = fills(&ev)[0];
    assert_eq!((fill.price, fill.spread_bps), (f("100000"), 0));

    h.price(BTC, "101000");
    let ev = h.market_order(a, BTC, Side::Sell, "0.1", "101000");
    let fill = fills(&ev)[0];
    assert_eq!((fill.realised_pnl, fill.profit_fee), (f("100"), f("10")));
    assert_eq!(h.balance(a), f("10090"));

    h.market_order(a, BTC, Side::Buy, "0.1", "101000");
    h.price(BTC, "100000");
    let ev = h.market_order(a, BTC, Side::Sell, "0.1", "100000");
    let fill = fills(&ev)[0];
    assert_eq!((fill.realised_pnl, fill.profit_fee), (f("-100"), Fixed::ZERO));
    assert_eq!(h.balance(a), f("9990"));
    h.assert_conserved(Fixed::ZERO);
}

#[test]
fn size_spread_charges_large_positions_and_splitting_does_not_avoid_it() {
    let mut h = Harness::new();
    let (whole, split) = (acct(1), acct(2));
    h.deposit(whole, "10000000");
    h.deposit(split, "10000000");
    h.price(BTC, "100000");

    // 3m notional: 0..1m free, 1m..3m at 2bps = $400 of spread.
    let ev = h.market_order(whole, BTC, Side::Buy, "30", "100000");
    let w = fills(&ev)[0];
    let whole_cost = (w.price - w.index).mul(w.qty);

    let ev = h.market_order(split, BTC, Side::Buy, "10", "100000");
    assert_eq!(fills(&ev)[0].spread_bps, 0);
    let ev = h.market_order(split, BTC, Side::Buy, "20", "100100");
    let s = fills(&ev)[0];
    assert_eq!(s.spread_bps, 2);
    let split_cost = (s.price - s.index).mul(s.qty);

    assert!((whole_cost - f("400")).abs() < f("0.00001"), "{whole_cost}");
    assert_eq!(split_cost, f("400"));

    // Reducing pays no size spread.
    let ev = h.market_order(split, BTC, Side::Sell, "30", "100000");
    assert_eq!(fills(&ev)[0].spread_bps, 0);
}

#[test]
fn quotes_are_indicative_and_slippage_is_bounded() {
    let mut h = Harness::new();
    let a = acct(1);
    h.deposit(a, "100000");
    h.price(BTC, "100000");

    // 10bps worse than quoted: inside the 20bps bound, fills at the live price.
    let ev = h.market_order(a, BTC, Side::Buy, "0.1", "99900");
    assert_eq!(fills(&ev)[0].price, f("100000"));

    // 30bps worse: declined.
    let ev = h.market_order(a, BTC, Side::Buy, "0.1", "99700");
    assert!(matches!(rejection(&ev), Reject::Slippage { slippage_bps: 30, .. }));

    // The client can tighten the bound.
    let ev = h.order(a, BTC, Side::Buy, "0.1", OrderKind::Market { quoted: f("99900"), max_slippage_bps: Some(5) });
    assert!(matches!(rejection(&ev), Reject::Slippage { .. }));

    // Price improvement passes straight through.
    let ev = h.market_order(a, BTC, Side::Buy, "0.1", "100500");
    assert_eq!(fills(&ev)[0].price, f("100000"));
}

#[test]
fn stale_or_divergent_feeds_mean_no_fill() {
    let mut h = Harness::new();
    let a = acct(1);
    h.deposit(a, "100000");
    h.price(BTC, "100000");
    h.advance(1_500);
    let ev = h.market_order(a, BTC, Side::Buy, "0.1", "100000");
    assert!(matches!(rejection(&ev), Reject::NoPrice { .. }));

    h.quotes(BTC, &["100000", "101000"]);
    let ev = h.market_order(a, BTC, Side::Buy, "0.1", "100000");
    assert!(matches!(rejection(&ev), Reject::NoPrice { .. }));
}

#[test]
fn malformed_quotes_are_refused_without_affecting_good_ones() {
    let mut h = Harness::new();
    let ts = h.ts;
    let quotes = vec![quote("venue0", "100000", ts), quote("venue1", "100000", ts), quote("bad", "-1", i64::MIN)];
    let ev = h.cmd(Command::Prices { market: BTC, quotes });
    // The bad quote is reported, and the two good ones still price the market.
    assert!(ev.iter().any(|e| matches!(e, Event::PricesRejected { market: BTC, .. })));
    assert_eq!(h.engine.markets()[&BTC].mark, Some(f("100000")));
    assert!(!h.engine.markets()[&BTC].quotes.contains_key("bad"));
}

#[test]
fn an_older_quote_does_not_replace_the_venues_stored_one() {
    let mut h = Harness::new();
    h.price(BTC, "100000");

    // In the same sequenced millisecond, a delayed venue0 quote stamped 100ms earlier arrives. The
    // stored one, stamped exactly at the sequenced time, counts and is kept.
    let ev = h.cmd(Command::Prices { market: BTC, quotes: vec![quote("venue0", "100100", h.ts - 100)] });
    assert!(!ev.iter().any(|e| matches!(e, Event::PricesRejected { .. })), "{ev:?}");
    let stored = &h.engine.markets()[&BTC].quotes["venue0"];
    assert_eq!((stored.mid, stored.ts), (f("100000"), h.ts));
    assert_eq!(h.engine.markets()[&BTC].mark, Some(f("100000")));
}

#[test]
fn a_quote_stamped_an_hour_ahead_is_refused_and_the_venue_counts_again_on_its_next_quote() {
    let mut h = Harness::new();
    let a = acct(1);
    h.deposit(a, "100000");
    h.price(BTC, "100000");

    // venue0's clock jumps an hour ahead for one quote.
    h.advance(100);
    let ev = h.cmd(Command::Prices { market: BTC, quotes: vec![quote("venue0", "100000", h.ts + 3_600_000)] });
    assert!(
        ev.iter().any(|e| matches!(e, Event::PricesRejected { market: BTC, reason } if reason.contains("venue0"))),
        "{ev:?}"
    );

    // Correctly stamped quotes follow. Two venues are needed, so the mark at their mean shows venue0
    // counts, and orders fill again.
    h.advance(100);
    h.quotes(BTC, &["100000", "100100"]);
    assert_eq!(h.engine.markets()[&BTC].mark, Some(f("100050")));
    let ev = h.market_order(a, BTC, Side::Buy, "0.1", "100050");
    assert_eq!(fills(&ev)[0].price, f("100050"));
}

#[test]
fn a_quote_is_kept_at_the_lead_limit_and_refused_one_millisecond_past_it() {
    let mut h = Harness::new();
    let limit = h.ts + MAX_QUOTE_LEAD_MS;
    let quotes = vec![quote("venue0", "100000", limit), quote("venue1", "100000", limit + 1)];
    let ev = h.cmd(Command::Prices { market: BTC, quotes });
    let refused: Vec<&str> = ev
        .iter()
        .filter_map(|e| match e {
            Event::PricesRejected { reason, .. } => Some(reason.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(refused, [format!("quote from venue1 stamped {}ms ahead of the sequenced time", MAX_QUOTE_LEAD_MS + 1)]);
    let stored: Vec<(&String, Millis)> = h.engine.markets()[&BTC].quotes.iter().map(|(s, q)| (s, q.ts)).collect();
    assert_eq!(stored, [(&"venue0".to_owned(), limit)]);
}

#[test]
fn a_kept_quote_stamped_ahead_does_not_hold_back_the_venues_next_quote() {
    let mut h = Harness::new();
    // Kept, but it does not count until the sequenced time reaches it.
    let ahead = h.ts + MAX_QUOTE_LEAD_MS;
    h.cmd(Command::Prices { market: BTC, quotes: vec![quote("venue0", "100000", ahead)] });

    // venue0's next quote is stamped earlier than the kept one, yet replaces it and prices the market
    // with venue1's.
    h.advance(100);
    h.quotes(BTC, &["100000", "100100"]);
    assert_eq!(h.engine.markets()[&BTC].mark, Some(f("100050")));
}

#[test]
fn a_refused_quote_also_drops_the_venues_older_quote() {
    let mut h = Harness::new();
    let a = acct(1);
    h.deposit(a, "100000");
    h.price(BTC, "100000");

    // venue0's quote from 100ms ago is still fresh, but after venue0 sends one stamped past the lead
    // limit it no longer counts, which leaves one venue of the two needed.
    h.advance(100);
    h.cmd(Command::Prices { market: BTC, quotes: vec![quote("venue0", "100000", h.ts + MAX_QUOTE_LEAD_MS + 1)] });
    let ev = h.market_order(a, BTC, Side::Buy, "0.1", "100000");
    assert!(matches!(rejection(&ev), Reject::NoPrice(_)));
}

#[test]
fn out_of_range_order_numbers_are_rejected_not_panicked_on() {
    let mut h = Harness::new();
    let a = acct(1);
    h.deposit(a, "100000");
    h.price(BTC, "100000");
    // Each of these would overflow `i128` arithmetic if it reached pricing.
    let huge = "1000000000.000000001";
    for (qty, quoted) in [(huge, "100000"), ("0.1", huge), ("0", "100000"), ("-1", "100000")] {
        let ev = h.market_order(a, BTC, Side::Buy, qty, quoted);
        assert!(matches!(rejection(&ev), Reject::InvalidOrder { .. }), "{qty} @ {quoted}: {ev:?}");
    }
    let ev = h.order(a, BTC, Side::Buy, "0.1", OrderKind::Limit { price: f(huge) });
    assert!(matches!(rejection(&ev), Reject::InvalidOrder { .. }));
}

#[test]
fn toxic_flow_pays_a_wider_spread() {
    let mut h = Harness::new();
    let a = acct(1);
    h.deposit(a, "100000");
    h.price(BTC, "100000");
    h.market_order(a, BTC, Side::Buy, "0.1", "100000");
    // 1s later the price is 50bps higher: markout +50, EWMA 25 >= 20.
    h.advance(1_000);
    h.price(BTC, "100500");
    assert_eq!(h.engine.accounts()[&a].toxicity_bps, 25);

    let ev = h.market_order(a, BTC, Side::Sell, "0.1", "100500");
    assert_eq!(fills(&ev)[0].spread_bps, 3);
}

#[test]
fn stress_rises_automatically_and_the_operator_can_raise_but_not_suppress_it() {
    let mut h = Harness::new();
    let a = acct(1);
    h.deposit(a, "100000");

    // 15bps venue divergence trips level 1: every fill pays at least 5bps.
    let ev = h.quotes(BTC, &["100000", "100150"]);
    assert!(ev.contains(&Event::StressChanged { market: BTC, level: 1 }));
    let ev = h.market_order(a, BTC, Side::Buy, "0.1", "100075");
    assert_eq!(fills(&ev)[0].spread_bps, 5);

    // The operator sets floor 0, but the automatic level holds.
    h.admin(AdminCommand::SetStressFloor { market: None, level: 0 });
    assert_eq!(h.engine.markets()[&BTC].stress.level, 1);

    // Calm again, but inside the cooldown: still stressed.
    h.advance(1_000);
    h.price(BTC, "100000");
    assert_eq!(h.engine.markets()[&BTC].stress.level, 1);
    // After the cooldown it steps down.
    h.advance(300_000);
    let ev = h.price(BTC, "100000");
    assert!(ev.contains(&Event::StressChanged { market: BTC, level: 0 }));

    // The operator can force it up.
    h.admin(AdminCommand::SetStressFloor { market: Some(BTC), level: 1 });
    assert_eq!(h.engine.markets()[&BTC].stress.level, 1);
}

#[test]
fn limit_orders_never_fill_worse_than_the_limit() {
    let mut h = Harness::new();
    let a = acct(1);
    h.deposit(a, "100000");
    h.price(BTC, "100000");
    let ev = h.order(a, BTC, Side::Buy, "0.1", OrderKind::Limit { price: f("99000") });
    assert!(fills(&ev).is_empty());
    let ev = h.price(BTC, "98500");
    assert_eq!(fills(&ev)[0].price, f("98500"));
}

// ── Helpers ──

/// A venue quote.
fn quote(source: &str, mid: &str, ts: Millis) -> SourceQuote {
    SourceQuote { source: source.into(), mid: f(mid), ts }
}
