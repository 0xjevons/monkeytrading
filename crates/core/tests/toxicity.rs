//! Toxicity scoring from `V11`: a score carries nine decimals of a bp and is the notional-weighted
//! average of its account's markouts, from a neutral prior over at most a memory of notional, each
//! taken at an index valid when it is scored, end to end through `Engine::apply`. Each issue's
//! scenario also runs under `V8`, which shows the rule it replaces.

mod common;

use common::events::fills;
use common::stress::with_divergence_shares;
use common::{AAPL, BTC, DE40, ETH, EURUSD, Harness, USDJPY, acct, f, ny, operator, params};
use engine_core::auth::ClientAuth;
use engine_core::command::{AdminCommand, Command, Sequenced};
use engine_core::event::{Event, Fill};
use engine_core::fixed::Rounding;
use engine_core::params::{NotionalWeighting, ProtocolParams, ToxicityParams, ToxicityTier};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, MarketId, Side};
use engine_core::{Engine, Fixed};

#[test]
fn a_steady_nine_bps_edge_reaches_the_first_tier() {
    // A 10% EWMA weight and tiers at 5 and 15 bps. In whole bps each step is 0.9, which truncates to
    // nothing. From `V11` the score averages 9 bps markouts with 10,000 of neutral history: 4.5
    // after the first 10,000 fill and 6 after the second, so the third pays the 5 bps tier's add-on.
    let toxicity = ToxicityParams {
        ewma_weight_bps: 1_000,
        tiers: vec![
            ToxicityTier { min_score_bps: 5, extra_spread_bps: 1 },
            ToxicityTier { min_score_bps: 15, extra_spread_bps: 3 },
        ],
        ..params().toxicity
    };
    let mut add_ons = [true; 9];
    add_ons[..2].fill(false);
    for (genesis, paid) in [(Harness::v8(ClientAuth::Unsigned), [false; 9]), (Harness::v11(), add_ons)] {
        let mut h = genesis;
        h.operator_admin(AdminCommand::SetParams(ProtocolParams { toxicity: toxicity.clone(), ..params() }));
        let a = funded(&mut h, 1, "100000");
        // Whether each fill paid more than the index: only a toxicity add-on prices these above it.
        let above: Vec<bool> = (0..9)
            .map(|_| marked_out(&mut h, a, BTC, Side::Buy, "0.1", "9"))
            .map(|fill| fill.price > fill.index)
            .collect();
        assert_eq!(above, paid, "{:?}", h.engine.protocol_version());
    }
}

#[test]
fn a_sub_bp_markout_moves_the_score_by_its_fills_share_of_the_history() {
    // A 10,000 fill against a prior of 10,000, 30,000, or 90,000 takes a half, a quarter, or a tenth
    // of its markout.
    let cases = [
        ("10000", "0.5", "0.25"),
        ("10000", "9", "4.5"),
        ("10000", "-0.25", "-0.125"),
        ("30000", "0.5", "0.125"),
        ("30000", "9", "2.25"),
        ("30000", "-0.25", "-0.0625"),
        ("90000", "0.5", "0.05"),
        ("90000", "9", "0.9"),
        ("90000", "-0.25", "-0.025"),
    ];
    for (prior, edge, expected) in cases {
        let mut h = Harness::v11();
        let notional = Some(NotionalWeighting { prior: f(prior), memory: f("10000000") });
        let toxicity = ToxicityParams { notional, ..params().toxicity };
        h.operator_admin(AdminCommand::SetParams(ProtocolParams { toxicity, ..params() }));
        let a = funded(&mut h, 1, "100000");
        marked_out(&mut h, a, BTC, Side::Buy, "0.1", edge);
        assert_eq!(score(&h, a), f(expected), "prior {prior}, edge {edge}");
    }
}

#[test]
fn dust_fills_cannot_move_a_score_out_of_its_tier() {
    // Before `V11` the dust markouts, -2 and -3 bps, take the score to 12 and then 5; from `V11` they
    // move it by less than a millionth of a bp.
    let cases =
        [(Harness::v8(ClientAuth::Unsigned), ("5", "5"), 0), (Harness::v11(), ("24.999999", "24.999999999"), 3)];
    for (genesis, (least, most), next_spread) in cases {
        let mut h = genesis;
        let version = h.engine.protocol_version();
        let a = funded(&mut h, 1, "100000");
        // A 10,000 buy marked out 50 bps up scores 25, inside the 20 bps tier.
        marked_out(&mut h, a, BTC, Side::Buy, "0.1", "50");
        assert_eq!(score(&h, a), f("25"));
        // A dust buy and sell at a flat price, each paying the 3 bps add-on, marked out together.
        let mid = mark(&h, BTC).to_string();
        h.price(BTC, &mid);
        for side in [Side::Buy, Side::Sell] {
            assert_eq!(one_fill(h.market_order(a, BTC, side, "0.000000001", &mid)).spread_bps, 3);
        }
        h.advance(1_000);
        h.price(BTC, &mid);
        let after_dust = score(&h, a);
        assert!(f(least) <= after_dust && after_dust <= f(most), "{version:?}: {after_dust}");
        let next = one_fill(h.market_order(a, BTC, Side::Buy, "1", &mid));
        assert_eq!(next.spread_bps, next_spread, "{version:?}");
    }
}

#[test]
fn a_markouts_weight_grows_with_its_fills_notional_but_never_takes_the_whole_score() {
    // A 50 bps markout against 10,000 of neutral history, on fills from dust to 990,000.
    let cases = [("0.000000001", "0.0000005"), ("0.1", "25"), ("0.3", "37.5"), ("0.9", "45"), ("9.9", "49.5")];
    for (qty, expected) in cases {
        let mut h = Harness::v11();
        let a = funded(&mut h, 1, "1000000");
        marked_out(&mut h, a, BTC, Side::Buy, qty, "50");
        assert_eq!(score(&h, a), f(expected), "{qty}");
    }
    // The same 0.1 split into ten fills, marked out together, moves the score as far as whole, to
    // within the rounding of each step.
    let mut h = Harness::v11();
    let a = funded(&mut h, 1, "1000000");
    h.price(BTC, "100000");
    (0..10).for_each(|_| {
        one_fill(h.market_order(a, BTC, Side::Buy, "0.01", "100000"));
    });
    h.advance(1_000);
    h.price(BTC, "100500");
    let split = score(&h, a);
    assert!(f("25") <= split && split <= f("25.00000001"), "{split}");
}

#[test]
fn a_markout_weighs_its_fills_notional_in_usd() {
    // A 50 bps markout against 10,000 of neutral history. 0.5 DE40 at 20,000 euros is 12,500 at
    // 1.25 dollars to the euro, and 100 USD-JPY at 150 yen is 100: each weighs its notional in USD.
    let cases = [(DE40, "0.5", "20000", "20100", "27.777777778"), (USDJPY, "100", "150", "150.75", "0.495049505")];
    for (market, qty, price, moved, expected) in cases {
        let mut h = Harness::fx();
        h.admin(AdminCommand::SetOperator(operator()));
        h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V11 });
        let a = funded(&mut h, 1, "100000");
        h.price(EURUSD, "1.25");
        h.price(market, price);
        one_fill(h.market_order(a, market, Side::Buy, qty, price));
        // EUR-USD stays fresh to the horizon, when the move scores the fill.
        h.advance(500);
        h.price(EURUSD, "1.25");
        h.advance(500);
        h.price(market, moved);
        assert_eq!(score(&h, a), f(expected), "{market}");
    }
}

#[test]
fn mixed_fill_sizes_settle_at_their_notional_weighted_markout() {
    // In turn, 1 BTC marked out 20 bps up and 0.1 BTC marked out flat, past the 10m memory: the
    // score settles within 0.05 bps of the fills' own notional-weighted average, about 18.18.
    let mut h = Harness::v11();
    let a = funded(&mut h, 1, "1000000");
    let (mut weighted, mut notional) = (Fixed::ZERO, Fixed::ZERO);
    for _ in 0..60 {
        // Long 1, then 0.9, short 0.1, and flat again.
        for (side, qty, edge) in
            [(Side::Buy, "1", 20), (Side::Sell, "0.1", 0), (Side::Sell, "1", 20), (Side::Buy, "0.1", 0)]
        {
            let fill = marked_out(&mut h, a, BTC, side, qty, &edge.to_string());
            weighted += fill.qty.mul(fill.price).mul_int(edge);
            notional += fill.qty.mul(fill.price);
        }
    }
    let average = weighted.div(notional);
    let settled = score(&h, a);
    assert!((settled - average).abs() < f("0.05"), "score {settled}, average {average}");
}

#[test]
fn a_shortened_horizon_holds_no_markout_behind_an_earlier_fill() {
    let mut h = Harness::v11();
    let (a, b) = (funded(&mut h, 1, "100000"), funded(&mut h, 2, "100000"));
    let toxicity = ToxicityParams { horizon_ms: 10_000, ..params().toxicity };
    h.operator_admin(AdminCommand::SetParams(ProtocolParams { toxicity, ..params() }));
    h.price(BTC, "100000");
    one_fill(h.market_order(a, BTC, Side::Buy, "0.1", "100000"));
    // Back to a 1 s horizon: b's markout falls due nine seconds before a's.
    h.operator_admin(AdminCommand::SetParams(params()));
    one_fill(h.market_order(b, BTC, Side::Buy, "0.1", "100000"));
    h.advance(1_000);
    h.price(BTC, "100500");
    assert_eq!((score(&h, a), score(&h, b), pending(&h)), (f("0"), f("25"), 1));
    h.advance(9_000);
    h.price(BTC, "100500");
    assert_eq!((score(&h, a), pending(&h)), (f("25"), 0));
}

#[test]
fn a_markout_waits_through_a_no_price_window_for_the_next_valid_index() {
    // A 2s horizon over 1s quotes: at the tick at 2s the fill's quotes are stale.
    let toxicity = ToxicityParams { horizon_ms: 2_000, ..params().toxicity };
    for (genesis, scored) in [(Harness::v8(ClientAuth::Unsigned), f("0")), (Harness::v11(), f("25"))] {
        let mut h = genesis;
        h.operator_admin(AdminCommand::SetParams(ProtocolParams { toxicity: toxicity.clone(), ..params() }));
        let a = funded(&mut h, 1, "100000");
        h.price(BTC, "100000");
        one_fill(h.market_order(a, BTC, Side::Buy, "0.1", "100000"));
        h.advance(2_000);
        h.cmd(Command::Tick);
        assert_eq!(score(&h, a), f("0"));
        // Before `V11` the tick scored it at the stale mark; from `V11` the next fresh index does.
        h.advance(1);
        h.price(BTC, "100500");
        assert_eq!(score(&h, a), scored, "{:?}", h.engine.protocol_version());
    }
}

#[test]
fn a_markout_with_no_valid_index_within_a_horizon_of_falling_due_moves_nothing() {
    // Due 1s after the fill; the first fresh index comes at the end of the next second, or just past.
    for (wait_ms, scored) in [(2_000, f("25")), (2_001, f("0"))] {
        let mut h = Harness::v11();
        let a = funded(&mut h, 1, "100000");
        h.price(BTC, "100000");
        one_fill(h.market_order(a, BTC, Side::Buy, "0.1", "100000"));
        h.advance(wait_ms);
        h.price(BTC, "100500");
        assert_eq!(score(&h, a), scored, "{wait_ms}");
        assert_eq!(pending(&h), 0, "{wait_ms}");
    }
}

#[test]
fn a_markout_is_scored_only_where_its_market_has_a_valid_index() {
    // At its horizon the market has stale quotes, one fresh source of the two it needs, or venues
    // 99 bps apart. The markout waits, and the fresh index that follows scores it.
    for case in ["stale", "one source", "divergent"] {
        let mut h = Harness::v11();
        let a = funded(&mut h, 1, "100000");
        h.price(BTC, "100000");
        h.advance(500);
        one_fill(h.market_order(a, BTC, Side::Buy, "0.1", "100000"));
        h.advance(1_000);
        match case {
            "stale" => h.cmd(Command::Tick),
            "one source" => h.quotes(BTC, &["100250"]),
            _ => h.quotes(BTC, &["100000", "101000"]),
        };
        assert_eq!((score(&h, a), pending(&h)), (f("0"), 1), "{case}");
        h.advance(500);
        h.price(BTC, "100500");
        assert_eq!((score(&h, a), pending(&h)), (f("25"), 0), "{case}");
    }

    // A fill just before the close: venues still quote after it, but outside its session the
    // market has no price, so the markout is dropped by the open.
    let mut h = Harness::v11();
    let a = funded(&mut h, 1, "100000");
    h.advance_to(ny(2026, 10, 20, 15, 59) + 59_500);
    h.price(AAPL, "200");
    one_fill(h.market_order(a, AAPL, Side::Buy, "50", "200"));
    h.advance(1_000);
    h.price(AAPL, "201");
    assert_eq!((score(&h, a), pending(&h)), (f("0"), 1));
    h.advance_to(ny(2026, 10, 21, 9, 30));
    h.price(AAPL, "201");
    assert_eq!((score(&h, a), pending(&h)), (f("0"), 0));
}

#[test]
fn a_markout_waiting_for_its_market_holds_back_none_behind_it() {
    let mut h = Harness::v11();
    let (a, b) = (funded(&mut h, 1, "100000"), funded(&mut h, 2, "100000"));
    h.price(BTC, "100000");
    h.price(ETH, "2500");
    h.advance(500);
    one_fill(h.market_order(a, BTC, Side::Buy, "0.1", "100000"));
    one_fill(h.market_order(b, ETH, Side::Buy, "4", "2500"));
    // At the horizon only ETH has a fresh index: b's markout is scored past a's, which waits.
    h.advance(1_000);
    h.price(ETH, "2512.5");
    assert_eq!((score(&h, a), score(&h, b), pending(&h)), (f("0"), f("25"), 1));
    h.advance(500);
    h.price(BTC, "100500");
    assert_eq!((score(&h, a), pending(&h)), (f("25"), 0));
}

#[test]
fn the_state_root_holds_scores_and_every_markout_the_queue_moves() {
    // A markout inserted ahead of a later-due one, one put back to wait while one behind it scores,
    // and a score with a fraction of a bp: after each command the root equals a rebuild's.
    let mut h = Harness::v11();
    let (a, b, c) = (funded(&mut h, 1, "100000"), funded(&mut h, 2, "100000"), funded(&mut h, 3, "100000"));
    h.price(BTC, "100000");
    h.advance(500);
    h.price(ETH, "2500");
    let toxicity = ToxicityParams { horizon_ms: 10_000, ..params().toxicity };
    h.operator_admin(AdminCommand::SetParams(ProtocolParams { toxicity, ..params() }));
    one_fill(h.market_order(a, BTC, Side::Buy, "0.1", "100000"));
    h.operator_admin(AdminCommand::SetParams(params()));
    // c's and then b's markouts go in ahead of a's, which falls due nine seconds after theirs.
    one_fill(h.market_order(c, BTC, Side::Buy, "0.1", "100000"));
    assert_root_matches_rebuild(&mut h.engine);
    one_fill(h.market_order(b, ETH, Side::Buy, "4", "2500"));
    assert_root_matches_rebuild(&mut h.engine);
    // At the horizon BTC's quotes are stale: c's markout waits, and b's, behind it, scores 0.5.
    h.advance(1_000);
    h.price(ETH, "2500.25");
    assert_eq!((score(&h, b), score(&h, c), pending(&h)), (f("0.5"), f("0"), 2));
    assert_root_matches_rebuild(&mut h.engine);
    h.advance(500);
    h.price(BTC, "100500");
    assert_eq!((score(&h, c), pending(&h)), (f("25"), 1));
    assert_root_matches_rebuild(&mut h.engine);
    h.advance(9_000);
    h.price(BTC, "100500");
    assert_eq!((score(&h, a), pending(&h)), (f("25"), 0));
    assert_root_matches_rebuild(&mut h.engine);
}

#[test]
fn parameters_from_v11_need_notional_weighting() {
    let unweighted = ToxicityParams { notional: None, ..params().toxicity };
    let set = AdminCommand::SetParams(ProtocolParams { toxicity: unweighted, ..params() });
    assert_eq!(Harness::v8(ClientAuth::Unsigned).operate(set.clone()), [Event::AdminApplied]);
    let reason = "toxicity needs notional weighting from protocol V11".to_string();
    assert_eq!(Harness::v11().operate(set), [Event::AdminRejected { reason }]);
}

#[test]
fn v11_activates_only_over_parameters_with_notional_weighting() {
    let mut h = Harness::v8(ClientAuth::Unsigned);
    let unweighted = ToxicityParams { notional: None, ..params().toxicity };
    h.operator_admin(AdminCommand::SetParams(ProtocolParams { toxicity: unweighted, ..params() }));
    let activate = Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V11 });
    let reason = "toxicity needs notional weighting from protocol V11";
    assert_eq!(h.cmd(activate.clone()), [Event::AdminRejected { reason: reason.into() }]);
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V8);
    // The node gives the parameters its weighting unsigned, funds credited or not; weighting that
    // fails its check is refused.
    let weighting = |prior: &str| NotionalWeighting { prior: f(prior), memory: f("10000000") };
    let set = |prior| Command::Admin(AdminCommand::SetToxicityNotional(weighting(prior)));
    assert!(matches!(&h.cmd(set("0"))[..], [Event::AdminRejected { .. }]));
    assert_eq!(h.cmd(set("10000")), [Event::AdminApplied]);
    let already = "the parameters in force already weigh markouts by notional".to_string();
    assert_eq!(h.cmd(set("20000")), [Event::AdminRejected { reason: already }]);
    assert_eq!(h.cmd(activate), [Event::AdminApplied]);
    assert_eq!(h.engine.params().and_then(|params| params.toxicity.notional), Some(weighting("10000")));
}

#[test]
fn a_whole_bps_score_carries_over_when_v11_activates() {
    let mut h = Harness::v8(ClientAuth::Unsigned);
    let a = funded(&mut h, 1, "100000");
    marked_out(&mut h, a, BTC, Side::Buy, "0.1", "50");
    // A markout queued before the activation carries no notional, so it is dropped unscored.
    h.price(BTC, "100500");
    one_fill(h.market_order(a, BTC, Side::Sell, "0.1", "100500"));
    assert_eq!(pending(&h), 1);
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V11 });
    assert_eq!((score(&h, a), pending(&h)), (f("25"), 0));
    // A score from before `V11` counts as a whole memory of history: from 25, a buy of 10,003 at the
    // 3 bps add-on, marked out 1 bp up, moves it about a thousandth of the way.
    h.advance(1_000);
    h.price(BTC, "100000");
    marked_out(&mut h, a, BTC, Side::Buy, "0.1", "1");
    assert_eq!(score(&h, a), f("24.976016791"));
}

#[test]
fn set_toxicity_notional_before_parameters_are_set_is_refused() {
    // The node sequences `SetToxicityNotional` only over parameters set before V11 that lack
    // notional weighting, and `carry_toxicity_notional` returns early when parameters are absent,
    // so a bare engine reaches this refusal only on a log the node did not write.
    let weighting = NotionalWeighting { prior: f("10000"), memory: f("10000000") };
    let cmd = Sequenced { seq: 0, ts: 0, cmd: Command::Admin(AdminCommand::SetToxicityNotional(weighting)) };
    let events = Engine::new().apply(&cmd).expect("a sequenced command applies");
    assert_eq!(events, [Event::AdminRejected { reason: "protocol params not set".into() }]);
}

#[test]
fn lowered_memory_clamps_toxicity_history_from_the_next_markout() {
    // Memory 100,000, then 10,000. A 10,000 fill at 100,000 marked out 50 bps up scores 25 over
    // 20,000 of history; a second 10,000 fill there, marked out flat, then weighs it. From `V16`
    // the lowered memory has clamped the history to 10,000, so the score falls by half its
    // distance to the markout (12.5); before, it keeps weighing 20,000 and falls only to
    // 16.666666667.
    for (genesis, clamped) in [(Harness::v14(), false), (Harness::v16(), true)] {
        let mut h = genesis;
        let version = h.engine.protocol_version();
        let set = |memory: &str| {
            let base = with_divergence_shares(params());
            let notional = Some(NotionalWeighting { prior: f("10000"), memory: f(memory) });
            let toxicity = ToxicityParams { notional, tiers: vec![], ..base.toxicity };
            AdminCommand::SetParams(ProtocolParams { toxicity, ..base })
        };
        h.operator_admin(set("100000"));
        let a = funded(&mut h, 1, "100000");
        marked_out(&mut h, a, BTC, Side::Buy, "0.1", "50");
        assert_eq!((score(&h, a), history(&h, a)), (f("25"), f("20000")), "{version:?}");
        h.operator_admin(set("10000"));
        let kept = if clamped { f("10000") } else { f("20000") };
        assert_eq!((score(&h, a), history(&h, a)), (f("25"), kept), "{version:?}");
        h.price(BTC, "100000");
        marked_out(&mut h, a, BTC, Side::Buy, "0.1", "0");
        let weighed = if clamped { f("12.5") } else { f("16.666666667") };
        assert_eq!((score(&h, a), history(&h, a)), (weighed, f("10000")), "{version:?}");
    }
}

// ── Helpers ──

/// Account `n`, funded with `amount`.
fn funded(h: &mut Harness, n: u8, amount: &str) -> AccountId {
    let account = acct(n);
    h.deposit(account, amount);
    account
}

/// Prices `market` at its mark (100,000 for BTC before it has one), fills `qty` for `account`
/// there, then a horizon later prices it `edge_bps` in the trader's favour from the fill price.
fn marked_out(h: &mut Harness, account: AccountId, market: MarketId, side: Side, qty: &str, edge_bps: &str) -> Fill {
    let mid = h.engine.markets()[&market].mark.unwrap_or(f("100000")).to_string();
    h.price(market, &mid);
    let fill = one_fill(h.market_order(account, market, side, qty, &mid));
    h.advance(1_000);
    let edge = fill.price.mul_div(f(edge_bps), Fixed::from_int(10_000), Rounding::Down).mul_int(side.sign());
    h.price(market, &(fill.price + edge).to_string());
    fill
}

/// The account's toxicity score.
fn score(h: &Harness, account: AccountId) -> Fixed {
    h.engine.accounts()[&account].toxicity_score()
}

/// The fill notional of history behind the account's score.
fn history(h: &Harness, account: AccountId) -> Fixed {
    h.engine.accounts()[&account].toxicity_history
}

/// The market's last mark.
fn mark(h: &Harness, market: MarketId) -> Fixed {
    h.engine.markets()[&market].mark.expect("a marked market")
}

/// How many markouts wait to be scored, read from the state encoding.
fn pending(h: &Harness) -> usize {
    let state = serde_json::to_value(&h.engine).expect("engine state serialises");
    state["markouts"].as_array().expect("a markout queue").len()
}

/// Panics unless the engine's state root, computed from what changed, equals the root of the same
/// state hashed from scratch.
fn assert_root_matches_rebuild(engine: &mut Engine) {
    let root = engine.state_root();
    let mut rebuilt: Engine = serde_json::from_slice(&serde_json::to_vec(engine).unwrap()).unwrap();
    assert_eq!(rebuilt.state_root(), root, "the incremental root differs from a rebuild's");
}

/// The one fill among `events`; panics if there is not exactly one.
fn one_fill(events: Vec<Event>) -> Fill {
    match &fills(&events)[..] {
        [fill] => (*fill).clone(),
        _ => panic!("expected one fill, got {events:?}"),
    }
}
