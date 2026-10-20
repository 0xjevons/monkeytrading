//! Protocol behaviour, end to end through `Engine::apply`.

use chrono::TimeZone;
use engine_core::command::{AdminCommand, Command, OrderKind, OrderRequest, Sequenced};
use engine_core::event::{CancelReason, Event, Fill, Reject};
use engine_core::market::{FinancingSpec, MarketSpec, PriceRules, Tier};
use engine_core::params::{ProtocolParams, Rollover, StressLevel, StressTrigger, ToxicityParams, ToxicityTier};
use engine_core::price::SourceQuote;
use engine_core::session::Schedule;
use engine_core::types::{AccountId, AssetClass, MarketId, Millis, Side};
use engine_core::{Engine, Fixed};

const BTC: MarketId = 1;
const ETH: MarketId = 2;
const AAPL: MarketId = 3;

fn f(s: &str) -> Fixed {
    s.parse().unwrap()
}

fn acct(n: u8) -> AccountId {
    AccountId([n; 20])
}

fn params() -> ProtocolParams {
    ProtocolParams {
        profit_fee_bps: 1_000,
        closeout_level_bps: 5_000,
        house_total_cap_bps: 20_000,
        stress_levels: vec![
            StressLevel { size_spread_multiplier_bps: 10_000, min_spread_bps: 0, trigger: None },
            StressLevel {
                size_spread_multiplier_bps: 20_000,
                min_spread_bps: 5,
                trigger: Some(StressTrigger {
                    divergence_bps: Some(10),
                    move_bps: Some(200),
                    utilisation_bps: Some(8_000),
                }),
            },
        ],
        stress_move_window_ms: 60_000,
        stress_cooldown_ms: 300_000,
        toxicity: ToxicityParams {
            horizon_ms: 1_000,
            ewma_weight_bps: 5_000,
            tiers: vec![ToxicityTier { min_score_bps: 20, extra_spread_bps: 3 }],
        },
        rollover: Rollover { tz: chrono_tz::Europe::London, hour: 22, minute: 0 },
    }
}

fn market(id: MarketId, symbol: &str, class: AssetClass, schedule: Schedule, margin_bps: i64) -> MarketSpec {
    MarketSpec {
        id,
        symbol: symbol.into(),
        asset_class: class,
        schedule,
        margin_tiers: vec![Tier { up_to: None, bps: margin_bps }],
        spread_tiers: vec![
            Tier { up_to: Some(Fixed::from_int(1_000_000)), bps: 0 },
            Tier { up_to: Some(Fixed::from_int(5_000_000)), bps: 2 },
            Tier { up_to: None, bps: 5 },
        ],
        price_rules: PriceRules { max_age_ms: 1_000, max_divergence_bps: 50, min_sources: 2 },
        max_slippage_bps: 20,
        financing: FinancingSpec { benchmark: "SOFR".into(), markup_bps: 250, day_basis: 365 },
        house_cap_bps: 5_000,
        min_order_notional: Fixed::ZERO,
    }
}

fn us_equities() -> Schedule {
    let windows: Vec<_> = ["Mon", "Tue", "Wed", "Thu", "Fri"]
        .iter()
        .map(|d| serde_json::json!({ "open": format!("{d} 09:30"), "close": format!("{d} 16:00") }))
        .collect();
    serde_json::from_value(serde_json::json!({ "kind": "weekly", "tz": "America/New_York", "windows": windows }))
        .unwrap()
}

/// Tue 20 Oct 2026 10:00 New York (US equities open).
fn start() -> Millis {
    chrono_tz::America::New_York.with_ymd_and_hms(2026, 10, 20, 10, 0, 0).unwrap().timestamp_millis()
}

struct Harness {
    engine: Engine,
    log: Vec<Sequenced>,
    ts: Millis,
    deposits: Fixed,
}

impl Harness {
    fn new() -> Self {
        let mut h = Harness { engine: Engine::new(), log: vec![], ts: start(), deposits: Fixed::ZERO };
        h.admin(AdminCommand::SetParams(params()));
        h.admin(AdminCommand::UpsertMarket(market(BTC, "BTC-USD", AssetClass::Crypto, Schedule::AlwaysOpen, 1_000)));
        h.admin(AdminCommand::UpsertMarket(market(ETH, "ETH-USD", AssetClass::Crypto, Schedule::AlwaysOpen, 1_000)));
        h.admin(AdminCommand::UpsertMarket(market(AAPL, "AAPL", AssetClass::Equity, us_equities(), 2_000)));
        h.cmd(Command::HouseCapital { amount: Fixed::from_int(100_000_000), chain_ref: "genesis".into() });
        h
    }

    fn cmd(&mut self, cmd: Command) -> Vec<Event> {
        let s = Sequenced { seq: self.log.len() as u64, ts: self.ts, cmd };
        let ev = self.engine.apply(&s).expect("in sequence");
        self.log.push(s);
        ev
    }

    fn admin(&mut self, cmd: AdminCommand) {
        let ev = self.cmd(Command::Admin(cmd));
        assert!(ev.contains(&Event::AdminApplied), "admin rejected: {ev:?}");
    }

    fn advance(&mut self, ms: Millis) {
        self.ts += ms;
    }

    fn advance_to(&mut self, ts: Millis) {
        assert!(ts >= self.ts);
        self.ts = ts;
    }

    fn price(&mut self, market: MarketId, mid: &str) -> Vec<Event> {
        self.quotes(market, &[mid, mid])
    }

    fn quotes(&mut self, market: MarketId, mids: &[&str]) -> Vec<Event> {
        let quotes = mids
            .iter()
            .enumerate()
            .map(|(i, m)| SourceQuote { source: format!("venue{i}"), mid: f(m), ts: self.ts })
            .collect();
        self.cmd(Command::Prices { market, quotes })
    }

    fn deposit(&mut self, account: AccountId, amount: &str) {
        self.deposits += f(amount);
        self.cmd(Command::Deposit { account, amount: f(amount), chain_ref: "0xdead".into() });
    }

    fn order(&mut self, account: AccountId, market: MarketId, side: Side, qty: &str, kind: OrderKind) -> Vec<Event> {
        self.cmd(Command::PlaceOrder(OrderRequest {
            account,
            client_id: 7,
            market,
            side,
            qty: f(qty),
            kind,
            reduce_only: false,
        }))
    }

    fn market_order(
        &mut self,
        account: AccountId,
        market: MarketId,
        side: Side,
        qty: &str,
        quoted: &str,
    ) -> Vec<Event> {
        self.order(account, market, side, qty, OrderKind::Market { quoted: f(quoted), max_slippage_bps: None })
    }

    fn balance(&self, account: AccountId) -> Fixed {
        self.engine.accounts()[&account].balance
    }

    /// Cash is never created or destroyed: client balances plus house cash
    /// equal deposits plus house capital, less approved withdrawals.
    fn assert_conserved(&self, withdrawn: Fixed) {
        let balances: Fixed = self.engine.accounts().values().map(|a| a.balance).sum();
        let expected = self.deposits + Fixed::from_int(100_000_000) - withdrawn;
        assert_eq!(balances + self.engine.house_cash(), expected);
    }
}

fn fills(ev: &[Event]) -> Vec<&Fill> {
    ev.iter()
        .filter_map(|e| match e {
            Event::Fill(f) => Some(f),
            _ => None,
        })
        .collect()
}

fn rejection(ev: &[Event]) -> &Reject {
    ev.iter()
        .find_map(|e| match e {
            Event::OrderRejected { reason, .. } => Some(reason),
            _ => None,
        })
        .unwrap_or_else(|| panic!("expected a rejection, got {ev:?}"))
}

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
    assert!(matches!(rejection(&ev), Reject::NoPrice(_)));

    h.quotes(BTC, &["100000", "101000"]);
    let ev = h.market_order(a, BTC, Side::Buy, "0.1", "100000");
    assert!(matches!(rejection(&ev), Reject::NoPrice(_)));
}

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
    h.advance_to(chrono_tz::America::New_York.with_ymd_and_hms(2026, 10, 20, 16, 30, 0).unwrap().timestamp_millis());
    let ev = h.cmd(Command::Tick);
    assert!(ev.contains(&Event::MarketClosed { market: AAPL }));
    let ev = h.market_order(a, AAPL, Side::Buy, "1", "200");
    assert_eq!(rejection(&ev), &Reject::MarketClosed);
    // After-hours prints never trigger anything.
    assert!(fills(&h.price(AAPL, "180")).is_empty());

    // Wed open gaps down through the stop: fills at the open, not at 190.
    h.advance_to(chrono_tz::America::New_York.with_ymd_and_hms(2026, 10, 21, 9, 30, 0).unwrap().timestamp_millis());
    let ev = h.price(AAPL, "170");
    assert!(ev.contains(&Event::MarketOpened { market: AAPL }));
    let fill = fills(&ev)[0];
    assert_eq!((fill.price, fill.realised_pnl), (f("170"), f("-300")));
    assert!(h.engine.accounts()[&a].orders.is_empty());
}

#[test]
fn closeout_takes_the_largest_loser_first() {
    let mut h = Harness::new();
    let a = acct(1);
    h.deposit(a, "1000");
    h.price(BTC, "100000");
    h.price(ETH, "4000");
    h.market_order(a, BTC, Side::Buy, "0.04", "100000");
    h.market_order(a, ETH, Side::Buy, "1", "4000");

    h.price(BTC, "97000");
    h.price(ETH, "3500");
    assert_eq!(h.engine.accounts()[&a].positions.len(), 2);

    // Equity 280 < 50% of margin 728: ETH (-600) goes, BTC (-120) survives.
    let ev = h.price(ETH, "3400");
    assert!(ev.iter().any(|e| matches!(e, Event::CloseOut { .. })));
    let closed = fills(&ev);
    assert_eq!(closed.len(), 1);
    assert_eq!((closed[0].market, closed[0].liquidation), (ETH, true));
    assert!(h.engine.accounts()[&a].positions.contains_key(&BTC));
    h.assert_conserved(Fixed::ZERO);
}

#[test]
fn negative_balance_protection_writes_off_gaps_against_the_house() {
    let mut h = Harness::new();
    let a = acct(1);
    h.deposit(a, "1000");
    h.price(BTC, "100000");
    h.market_order(a, BTC, Side::Buy, "0.09", "100000");

    // A 20% gap also trips stress, so the close-out pays the 5bps stress
    // spread: sold at 79,960 for a loss of 1,803.6 on a 1,000 balance.
    let ev = h.price(BTC, "80000");
    assert!(ev.contains(&Event::BadDebt { account: a, amount: f("803.6") }), "{ev:?}");
    assert_eq!(h.balance(a), Fixed::ZERO);
    h.assert_conserved(Fixed::ZERO);
}

#[test]
fn withdrawals_are_limited_to_free_cash() {
    let mut h = Harness::new();
    let a = acct(1);
    h.deposit(a, "1000");
    h.price(BTC, "100000");
    h.market_order(a, BTC, Side::Buy, "0.09", "100000");

    let ev = h.cmd(Command::Withdraw { account: a, amount: f("200") });
    assert!(
        matches!(&ev[0], Event::WithdrawalRejected { reason: Reject::InsufficientFunds { available }, .. } if *available == f("100"))
    );
    let ev = h.cmd(Command::Withdraw { account: a, amount: f("100") });
    assert_eq!(ev[0], Event::WithdrawalApproved { account: a, amount: f("100"), nonce: 1 });
    h.assert_conserved(f("100"));
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
    h.advance_to(chrono::Utc.with_ymd_and_hms(2026, 10, 20, 21, 0, 1).unwrap().timestamp_millis());
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
fn house_exposure_is_capped_but_risk_reducing_flow_is_not() {
    let mut h = Harness::new();
    let (whale, other) = (acct(1), acct(2));
    h.deposit(whale, "100000000");
    h.deposit(other, "100000000");
    h.price(BTC, "100000");

    // Cap is 50% of 100m NAV: 60m of BTC is too much.
    let ev = h.market_order(whale, BTC, Side::Buy, "600", "100100");
    assert_eq!(rejection(&ev), &Reject::HouseCapacity);
    let ev = h.market_order(whale, BTC, Side::Buy, "400", "100100");
    assert_eq!(fills(&ev).len(), 1);
    // Taking the other side shrinks house exposure, so size is fine.
    let ev = h.market_order(other, BTC, Side::Sell, "600", "99900");
    assert_eq!(fills(&ev).len(), 1);
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

#[test]
fn triggered_orders_that_cannot_fill_are_cancelled() {
    let mut h = Harness::new();
    let a = acct(1);
    h.deposit(a, "100");
    h.price(BTC, "100000");
    h.order(a, BTC, Side::Buy, "1", OrderKind::Stop { price: f("101000") });
    let ev = h.price(BTC, "101000");
    assert!(ev.iter().any(|e| matches!(e, Event::OrderCancelled { reason: CancelReason::Unfillable, .. })));
}

#[test]
fn replaying_the_log_reproduces_the_state_root() {
    let mut h = Harness::new();
    let a = acct(1);
    h.deposit(a, "10000");
    h.price(BTC, "100000");
    h.market_order(a, BTC, Side::Buy, "0.05", "100000");
    h.order(a, BTC, Side::Sell, "0.05", OrderKind::Limit { price: f("105000") });
    h.price(BTC, "106000");

    let mut replica = Engine::new();
    for s in &h.log {
        replica.apply(s).unwrap();
    }
    assert_eq!(replica.state_root(), h.engine.state_root());

    // The log must be applied in order.
    let mut out_of_order = Engine::new();
    assert!(out_of_order.apply(&h.log[1]).is_err());
}
