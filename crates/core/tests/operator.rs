//! Operator commands through `Engine::apply`: admin commands signed by the operator the log records,
//! its nonces and expiries, the commands it may send, and every market reduce-only.

mod common;

use common::events::{fills, rejection};
use common::{AAPL, BTC, CHAIN_ID, Client, DE40, ETH, EURUSD, Harness, USDJPY, VAULT, acct, f, market, operator};
use engine_core::Engine;
use engine_core::auth::{ClientAuth, MAX_EXPIRY_AHEAD_MS, Operator};
use engine_core::command::{AdminCommand, AdminRequest, Command, OrderKind, Sequenced};
use engine_core::eip712;
use engine_core::event::{CancelReason, Event, Reject};
use engine_core::market::{MarketSpec, QuoteCurrency};
use engine_core::params::ProtocolParams;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::{Period, Schedule};
use engine_core::types::{AccountId, AssetClass, MarketId, Side};

/// A market id the harness genesis does not list.
const SOL: MarketId = 10;

#[test]
fn a_signed_admin_command_applies_and_uses_the_operators_next_nonce() {
    let mut h = operated();
    let events = h.cmd(Client::operator().admin(0, reduce_only(Some(BTC), true)));
    assert_eq!(events, [Event::AdminApplied]);
    assert!(h.engine.markets()[&BTC].reduce_only);
    assert_eq!(h.engine.next_request_nonce(operator().address), 1);
}

#[test]
fn a_repeated_admin_command_is_rejected_as_a_stale_nonce_and_lists_its_market_once() {
    let mut h = operated();
    let listing = Client::operator().admin(0, AdminCommand::UpsertMarket(sol()));
    assert_eq!(h.cmd(listing.clone()), [Event::MarketOpened { market: SOL }, Event::AdminApplied]);
    assert_eq!(h.cmd(listing), [rejected("invalid nonce: expected 1")]);
    assert_eq!(h.engine.next_request_nonce(operator().address), 1);
    assert_eq!(h.engine.markets()[&SOL].spec, sol());
}

#[test]
fn an_admin_command_signed_by_another_key_or_altered_is_rejected_and_uses_no_nonce() {
    let mut h = operated();
    let forged = Client { expires_at: h.ts + 60_000, ..Client::new(7) }.admin(0, reduce_only(Some(BTC), true));
    assert_eq!(h.cmd(forged), [rejected("invalid signature")]);
    let Command::SignedAdmin(mut altered) = Client::operator().admin(0, reduce_only(Some(BTC), true)) else {
        unreachable!("`admin` signs an admin command")
    };
    altered.command = reduce_only(Some(ETH), true);
    assert_eq!(h.cmd(Command::SignedAdmin(altered)), [rejected("invalid signature")]);
    assert_eq!(h.engine.next_request_nonce(operator().address), 0);
    assert!(h.engine.markets().values().all(|market| !market.reduce_only));
}

#[test]
fn an_admin_command_signed_for_another_vault_is_rejected() {
    let mut h = operated();
    let elsewhere = Client { domain: eip712::domain(CHAIN_ID, AccountId([0xbb; 20])), ..Client::operator() };
    assert_eq!(h.cmd(elsewhere.admin(0, reduce_only(Some(BTC), true))), [rejected("invalid signature")]);
    assert!(!h.engine.markets()[&BTC].reduce_only);
}

#[test]
fn an_admin_command_applies_up_to_its_expiry_and_not_after() {
    let mut h = operated();
    let at_expiry = Client { expires_at: h.ts, ..Client::operator() };
    assert_eq!(h.cmd(at_expiry.admin(0, reduce_only(Some(BTC), true))), [Event::AdminApplied]);
    let expired = Client { expires_at: h.ts - 1, ..Client::operator() };
    let events = h.cmd(expired.admin(1, reduce_only(Some(ETH), true)));
    assert_eq!(events, [rejected(&format!("expired at {}", h.ts - 1))]);
    let too_far = Client { expires_at: h.ts + MAX_EXPIRY_AHEAD_MS + 1, ..Client::operator() };
    let events = h.cmd(too_far.admin(2, reduce_only(Some(ETH), true)));
    assert_eq!(events, [rejected(&format!("expiry after the latest allowed, {}", h.ts + MAX_EXPIRY_AHEAD_MS))]);
    // Both refusals used their nonces, so neither command can be sequenced again.
    assert_eq!(h.engine.next_request_nonce(operator().address), 3);
    assert!(!h.engine.markets()[&ETH].reduce_only);
}

#[test]
fn the_operator_may_not_send_commands_only_the_node_sequences() {
    let mut h = operated();
    let other = Operator { address: acct(9), ..operator() };
    let node_only = [
        AdminCommand::SetClientAuth(ClientAuth::Unsigned),
        AdminCommand::SetOperator(other),
        AdminCommand::ActivateProtocol { version: ProtocolVersion::V8 },
    ];
    for (nonce, command) in (0..).zip(node_only) {
        assert!(!command.is_operator_command(), "{command:?}");
        let events = h.cmd(Client::operator().admin(nonce, command));
        assert_eq!(events, [rejected("the operator may not send this command")]);
    }
    assert_eq!(h.engine.next_request_nonce(operator().address), 3);
    assert_eq!(h.engine.operator(), Some(&operator()));
    assert_eq!(h.engine.client_auth(), Some(&ClientAuth::Eip712 { chain_id: CHAIN_ID, vault: VAULT }));
}

#[test]
fn every_operator_command_applies_through_the_signed_channel() {
    let mut h = operated();
    h.price(BTC, "100000");
    let fee = ProtocolParams { profit_fee_bps: 1_500, ..common::params() };
    let commands = [
        AdminCommand::SetParams(fee.clone()),
        AdminCommand::UpsertMarket(sol()),
        AdminCommand::SetBenchmarkRate { benchmark: "SOFR".into(), annual_bps: 430 },
        AdminCommand::SetStressFloor { market: Some(BTC), level: 1 },
        reduce_only(Some(ETH), true),
        reduce_only(None, true),
    ];
    for command in commands {
        assert!(command.is_operator_command(), "{command:?}");
        let events = h.operate(command.clone());
        assert_eq!(events.last(), Some(&Event::AdminApplied), "{command:?}: {events:?}");
    }
    assert_eq!(h.engine.params(), Some(&fee));
    assert!(h.engine.markets().contains_key(&SOL));
    assert_eq!(h.engine.markets()[&BTC].stress.level, 1);
    assert!(h.engine.markets()[&ETH].reduce_only);
    assert!(h.engine.markets().values().all(|market| h.engine.is_reduce_only(market)));
}

#[test]
fn a_calendar_update_closes_its_market_from_its_seq_on() {
    let mut h = operated();
    let a = h.signer(1);
    h.deposit(a, "100000");
    h.price(AAPL, "230");
    assert_eq!(fills(&h.market_order(a, AAPL, Side::Buy, "10", "230")).len(), 1);

    // The venue publishes a closure from now, 10:00 on Tue 20 Oct 2026, to today's close.
    let Schedule::Weekly { tz, windows, .. } = common::us_equities() else { unreachable!("US equities are weekly") };
    let at = |local: &str| local.parse().expect("a local date and time");
    let closure = Period { from: at("2026-10-20T10:00:00"), to: at("2026-10-20T16:00:00") };
    let calendar =
        Schedule::Weekly { tz, windows, closed_dates: vec![], closures: vec![closure], special_sessions: vec![] };
    let update = AdminCommand::UpsertMarket(market(AAPL, "AAPL", AssetClass::Equity, calendar, 2_000));
    let seq = h.log.len();
    assert_eq!(h.operate(update), [Event::MarketClosed { market: AAPL }, Event::AdminApplied]);
    assert_eq!(rejection(&h.market_order(a, AAPL, Side::Sell, "10", "230")), &Reject::MarketClosed);

    // Replayed up to the update, the market is still open under the earlier calendar.
    let mut before = Engine::new();
    h.log[..seq].iter().for_each(|s| {
        before.apply(s).expect("in sequence");
    });
    assert!(before.markets()[&AAPL].open);
}

#[test]
fn every_market_reduce_only_refuses_new_risk_and_fills_closing_trades() {
    let mut h = operated();
    let a = h.signer(1);
    h.deposit(a, "100000");
    h.price(BTC, "100000");
    h.price(ETH, "4000");
    h.market_order(a, BTC, Side::Buy, "0.2", "100000");
    h.market_order(a, ETH, Side::Sell, "2", "4000");
    let opening = OrderKind::Limit { price: f("99000") };
    h.order(a, BTC, Side::Buy, "0.1", opening);
    h.operator_admin(reduce_only(None, true));

    assert_eq!(rejection(&h.market_order(a, BTC, Side::Buy, "0.1", "100000")), &Reject::ReduceOnly);
    assert_eq!(rejection(&h.market_order(a, ETH, Side::Sell, "1", "4000")), &Reject::ReduceOnly);
    // A sell through the whole long would open a short: refused, not clipped.
    assert_eq!(rejection(&h.market_order(a, BTC, Side::Sell, "0.3", "100000")), &Reject::ReduceOnly);
    // A resting order that would add risk is cancelled when it triggers.
    let events = h.price(BTC, "98000");
    assert!(events.contains(&Event::OrderCancelled { account: a, order_id: 2, reason: CancelReason::Unfillable }));

    let closed = h.market_order(a, BTC, Side::Sell, "0.2", "98000");
    assert_eq!(fills(&closed)[0].position, f("0"), "{closed:?}");
    let reduced = h.market_order(a, ETH, Side::Buy, "0.5", "4000");
    assert_eq!(fills(&reduced)[0].position, f("-1.5"), "{reduced:?}");

    // A market listed while the mode is on takes no new risk either.
    h.operator_admin(AdminCommand::UpsertMarket(sol()));
    h.price(SOL, "150");
    assert_eq!(rejection(&h.market_order(a, SOL, Side::Buy, "1", "150")), &Reject::ReduceOnly);

    // Lifting it restores new risk everywhere but where a market keeps its own flag.
    h.operator_admin(reduce_only(Some(ETH), true));
    h.operator_admin(reduce_only(None, false));
    assert_eq!(fills(&h.market_order(a, SOL, Side::Buy, "1", "150")).len(), 1);
    assert_eq!(rejection(&h.market_order(a, ETH, Side::Sell, "1", "4000")), &Reject::ReduceOnly);
}

#[test]
fn a_close_out_still_runs_with_every_market_reduce_only() {
    let mut h = operated();
    let c = h.signer(3);
    h.deposit(c, "1000");
    h.price(BTC, "100000");
    h.market_order(c, BTC, Side::Buy, "0.09", "100000");
    h.operator_admin(reduce_only(None, true));
    // Equity 280 against a margin of 828: below the 50% close-out level.
    let events = h.price(BTC, "92000");
    let liquidations: Vec<_> = fills(&events).into_iter().filter(|fill| fill.liquidation).collect();
    assert_eq!(liquidations.len(), 1, "{events:?}");
    assert_eq!(liquidations[0].position, f("0"));
}

#[test]
fn unsigned_operator_commands_are_refused_under_v8_once_funds_are_credited() {
    // Genesis sequenced its operator commands unsigned, and they applied before it credited house
    // capital last.
    let mut h = operated();
    assert!(h.engine.params().is_some() && h.engine.markets().len() == 3);
    let unsigned = [
        AdminCommand::SetParams(ProtocolParams { profit_fee_bps: 1_500, ..common::params() }),
        AdminCommand::UpsertMarket(sol()),
        AdminCommand::SetBenchmarkRate { benchmark: "SOFR".into(), annual_bps: 430 },
        AdminCommand::SetStressFloor { market: None, level: 1 },
        reduce_only(Some(BTC), true),
        reduce_only(None, true),
    ];
    for command in unsigned {
        let events = h.cmd(Command::Admin(command.clone()));
        assert_eq!(events, [rejected("from protocol V8 the operator signs this command once funds are credited")]);
    }
    assert_eq!(h.engine.params(), Some(&common::params()));
    assert!(!h.engine.markets().contains_key(&SOL));
    assert!(h.engine.markets().values().all(|m| m.stress.level == 0 && !h.engine.is_reduce_only(m)));
    // Commands only the node sequences keep their own rules.
    let events = h.cmd(Command::Admin(AdminCommand::SetClientAuth(ClientAuth::Unsigned)));
    assert_eq!(events, [rejected("client auth is already recorded")]);
}

#[test]
fn a_deposit_also_ends_unsigned_operator_commands_but_v7_still_applies_them() {
    let mut engine = Engine::new();
    let mut apply = |cmd: Command| {
        let seq = engine.last_seq().map_or(0, |seq| seq + 1);
        engine.apply(&Sequenced { seq, ts: common::start(), cmd }).expect("in sequence")
    };
    for cmd in [
        AdminCommand::SetClientAuth(ClientAuth::Unsigned),
        AdminCommand::SetOperator(operator()),
        AdminCommand::ActivateProtocol { version: ProtocolVersion::V8 },
        AdminCommand::SetParams(common::params()),
        AdminCommand::UpsertMarket(sol()),
    ] {
        assert_eq!(apply(Command::Admin(cmd)).last(), Some(&Event::AdminApplied));
    }
    apply(Command::Deposit { account: acct(1), amount: f("100"), chain_ref: "0xdeposit".into() });
    let events = apply(Command::Admin(reduce_only(Some(SOL), true)));
    assert_eq!(events, [rejected("from protocol V8 the operator signs this command once funds are credited")]);

    let mut h = Harness::v7();
    h.admin(reduce_only(Some(BTC), true));
    assert!(h.engine.markets()[&BTC].reduce_only);
}

#[test]
fn from_v8_no_parameters_can_price_a_closing_trade_at_the_price_itself() {
    let mut h = operated();
    let a = h.signer(1);
    h.deposit(a, "100000");
    h.price(BTC, "100000");
    h.market_order(a, BTC, Side::Buy, "0.2", "100000");
    // Stress and toxicity spreads that add up to the whole price are refused, whichever carries it.
    let refused = [(10_000, 0), (9_000, 1_000), (0, 10_000)];
    for (stress, toxicity) in refused {
        let events = h.operate(AdminCommand::SetParams(spreads(stress, toxicity)));
        assert!(
            matches!(&events[..], [Event::AdminRejected { reason }] if reason.contains("reach the price")),
            "{events:?}"
        );
    }
    assert_eq!(h.engine.params(), Some(&common::params()));

    // A basis point under the price: the top level's spread applies, and the close still fills.
    h.operator_admin(AdminCommand::SetParams(spreads(9_999, 0)));
    h.operator_admin(AdminCommand::SetStressFloor { market: Some(BTC), level: 1 });
    let closed = h.market_order(a, BTC, Side::Sell, "0.2", "0.01");
    let fill = fills(&closed)[0];
    assert_eq!((fill.position, fill.spread_bps), (f("0"), 9_999), "{closed:?}");
    assert!(fill.price.is_positive());
}

#[test]
fn v8_activates_only_over_parameters_that_let_a_closing_trade_fill() {
    // Under V7 such parameters still apply, and a close at the top level is refused.
    let mut h = Harness::v7();
    h.admin(AdminCommand::SetParams(spreads(10_000, 0)));
    h.admin(AdminCommand::SetOperator(operator()));
    let events = h.cmd(Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V8 }));
    assert!(
        matches!(&events[..], [Event::AdminRejected { reason }] if reason.contains("reach the price")),
        "{events:?}"
    );
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V7);
    h.admin(AdminCommand::SetParams(common::params()));
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V8 });
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V8);
}

#[test]
fn signed_parameters_and_listings_carry_the_house_cap_floors_and_their_range_checks() {
    let mut h = operated();
    let floor = |amount: &str| ProtocolParams { house_total_cap_floor: f(amount), ..common::params() };
    h.operator_admin(AdminCommand::SetParams(floor("5000000")));
    assert_eq!(h.engine.params().map(|p| p.house_total_cap_floor), Some(f("5000000")));
    let events = h.operate(AdminCommand::SetParams(floor("-1")));
    assert_eq!(events, [rejected("house_total_cap_floor out of range")]);

    let listing = |amount: &str| MarketSpec { house_cap_floor: f(amount), ..sol() };
    h.operator_admin(AdminCommand::UpsertMarket(listing("1000000")));
    assert_eq!(h.engine.markets()[&SOL].spec.house_cap_floor, f("1000000"));
    let events = h.operate(AdminCommand::UpsertMarket(listing("-1")));
    assert_eq!(events, [rejected("house_cap_floor out of range")]);
    assert_eq!(h.engine.markets()[&SOL].spec.house_cap_floor, f("1000000"));
}

#[test]
fn signed_listings_follow_the_quote_currency_rules() {
    let mut h = operated();
    let eurusd = market(EURUSD, "EUR-USD", AssetClass::Fx, Schedule::AlwaysOpen, 200);
    h.operator_admin(AdminCommand::UpsertMarket(eurusd));
    h.operator_admin(AdminCommand::UpsertMarket(common::de40(500)));
    assert_eq!(h.engine.markets()[&DE40].spec.quote_currency, QuoteCurrency::Eur { fx_market: EURUSD });

    // USD-JPY in dollars, and Germany 40 converted at a market that is not EUR-USD, are refused.
    let usdjpy = market(USDJPY, "USD-JPY", AssetClass::Fx, Schedule::AlwaysOpen, 200);
    let via_btc = MarketSpec { quote_currency: QuoteCurrency::Eur { fx_market: BTC }, ..common::de40(500) };
    for listing in [usdjpy, via_btc] {
        let events = h.operate(AdminCommand::UpsertMarket(listing.clone()));
        assert!(matches!(&events[..], [Event::AdminRejected { .. }]), "{listing:?}: {events:?}");
    }
    assert!(!h.engine.markets().contains_key(&USDJPY));
    assert_eq!(h.engine.markets()[&DE40].spec.quote_currency, QuoteCurrency::Eur { fx_market: EURUSD });
}

#[test]
fn signed_admin_commands_and_every_market_reduce_only_need_v8() {
    let mut h = Harness::v7();
    h.admin(AdminCommand::SetOperator(operator()));
    let events = h.cmd(Client::operator().admin(0, reduce_only(Some(BTC), true)));
    assert_eq!(events, [rejected("signed admin commands need protocol V8")]);
    assert_eq!(h.engine.next_request_nonce(operator().address), 0);
    let events = h.cmd(Command::Admin(reduce_only(None, true)));
    assert_eq!(events, [rejected("reduce-only for every market needs protocol V8")]);
    assert!(h.engine.markets().values().all(|market| !h.engine.is_reduce_only(market)));
}

#[test]
fn v8_activates_only_once_the_operator_is_recorded() {
    let mut h = Harness::new();
    h.admin(AdminCommand::SetClientAuth(ClientAuth::Unsigned));
    let activate = Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V8 });
    assert_eq!(h.cmd(activate.clone()), [rejected("protocol V8 needs the operator recorded first")]);
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V2);
    h.admin(AdminCommand::SetOperator(operator()));
    assert_eq!(h.cmd(activate), [Event::AdminApplied]);
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V8);
}

#[test]
fn the_operator_is_recorded_once() {
    let mut h = operated();
    let other = Operator { address: acct(9), ..operator() };
    let events = h.cmd(Command::Admin(AdminCommand::SetOperator(other)));
    assert_eq!(events, [rejected("the operator is already recorded")]);
    assert_eq!(h.engine.operator(), Some(&operator()));
}

#[test]
fn v7_state_encodes_without_the_fields_v8_added() {
    let added = ["operator", "global_reduce_only"];
    let v7 = serde_json::to_value(&Harness::v7().engine).unwrap();
    assert!(added.iter().all(|field| v7.get(field).is_none()), "{added:?} in a V7 encoding");

    let mut h = operated();
    h.operator_admin(reduce_only(None, true));
    let v5 = serde_json::to_value(&h.engine).unwrap();
    assert!(added.iter().all(|field| v5.get(field).is_some()), "{added:?} missing from a V8 encoding");
    assert_eq!(v5["request_nonces"][operator().address.to_string()], 1);
}

#[test]
fn admin_requests_and_operator_records_refuse_fields_this_build_does_not_know() {
    let Command::SignedAdmin(req) = Client::operator().admin(0, reduce_only(None, true)) else {
        unreachable!("`admin` signs an admin command")
    };
    let mut body = serde_json::to_value(&req).unwrap();
    assert_eq!(serde_json::from_value::<AdminRequest>(body.clone()).unwrap(), *req);
    body["salt"] = 1.into();
    assert!(serde_json::from_value::<AdminRequest>(body).is_err());

    let mut record = serde_json::to_value(operator()).unwrap();
    assert_eq!(serde_json::from_value::<Operator>(record.clone()).unwrap(), operator());
    record["name"] = "ops".into();
    assert!(serde_json::from_value::<Operator>(record).is_err());
}

// ── Helpers ──

/// `V8` with signed requests under `CHAIN_ID` and `VAULT`, and the harness operator.
fn operated() -> Harness {
    Harness::v8(ClientAuth::Eip712 { chain_id: CHAIN_ID, vault: VAULT })
}

/// `SetReduceOnly` for `market`, or for every market.
fn reduce_only(market: Option<MarketId>, reduce_only: bool) -> AdminCommand {
    AdminCommand::SetReduceOnly { market, reduce_only }
}

/// The `AdminRejected` event with `reason`.
fn rejected(reason: &str) -> Event {
    Event::AdminRejected { reason: reason.into() }
}

/// The harness parameters with the stress level's minimum spread and the toxicity tier's extra
/// spread set to `stress` and `toxicity` bps.
fn spreads(stress: i64, toxicity: i64) -> ProtocolParams {
    let mut params = common::params();
    params.stress_levels[1].min_spread_bps = stress;
    params.toxicity.tiers[0].extra_spread_bps = toxicity;
    params
}

/// A 24/7 SOL listing the harness genesis does not have.
fn sol() -> MarketSpec {
    market(SOL, "SOL-USD", AssetClass::Crypto, Schedule::AlwaysOpen, 1_000)
}
