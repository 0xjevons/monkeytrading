//! A market quoted in another currency than USD has no price while its FX market has none: it deals
//! neither way, keeps its last mark whatever its own quotes say, triggers nothing, and close-out
//! passes it over. Each FX price is then a price update for the markets that convert at it, with
//! every mark it moves set before any market acts on one.

mod common;

use common::{BTC, DE40, EURUSD, Harness, acct, de40, f, fills, market, ny, only_fill, rejection, us_equities};
use engine_core::command::{AdminCommand, Command, OrderKind};
use engine_core::event::{Event, Reject};
use engine_core::market::{MarketSpec, PriceRules};
use engine_core::session::Schedule;
use engine_core::types::{AssetClass, MarketId, Side};

/// A second market quoted in euros at EUR-USD.
const FR40: MarketId = 8;

#[test]
fn nothing_deals_while_the_fx_market_has_no_price() {
    let (mut h, a) = (Harness::fx(), acct(1));
    let no_fx = Reject::NoFxPrice { fx_market: EURUSD };
    h.deposit(a, "100000");
    h.price(DE40, "20000");
    // EUR-USD has never had a price, so Germany 40 has no USD value yet.
    assert_eq!(rejection(&h.market_order(a, DE40, Side::Buy, "1", "20000")), &no_fx);
    h.price(EURUSD, "1.08");
    h.market_order(a, DE40, Side::Buy, "2", "20000");
    h.order(a, DE40, Side::Sell, "1", OrderKind::Limit { price: f("20150") });
    h.price(DE40, "20100");

    // EUR-USD goes stale while Germany 40 keeps quoting: Germany 40 has no price either, so it keeps
    // its last mark, the limit its quotes reach keeps resting, and nothing opens or closes.
    h.advance(1_500);
    h.price(DE40, "20200");
    assert_eq!(h.engine.markets()[&DE40].mark, Some(f("20100")));
    assert_eq!(h.engine.accounts()[&a].orders.len(), 1);
    assert_eq!(rejection(&h.market_order(a, DE40, Side::Buy, "1", "20200")), &no_fx);
    assert_eq!(rejection(&h.market_order(a, DE40, Side::Sell, "2", "20200")), &no_fx);
    // The position is valued at its last marks, 200 EUR at 1.08, and free cash withdraws.
    assert_eq!(h.equity_and_margin(a).0, f("100216"));
    let ev = h.cmd(Command::Withdraw { account: a, amount: f("1000"), auth: None });
    assert!(matches!(ev[..], [Event::WithdrawalApproved { .. }]), "{ev:?}");

    // An index that values a euro above 10 USD converts nothing either.
    h.price(EURUSD, "11");
    assert_eq!(h.engine.markets()[&DE40].fx_mark, Some(f("1.08")));
    assert_eq!(rejection(&h.market_order(a, DE40, Side::Buy, "1", "20200")), &no_fx);

    // EUR-USD's next valid price is Germany 40's price too: it marks from its fresh quotes and the
    // limit fills on that command.
    let ev = h.price(EURUSD, "1.09");
    assert_eq!(fills(&ev).len(), 1, "{ev:?}");
    assert_eq!(h.engine.markets()[&DE40].mark, Some(f("20200")));
    h.assert_conserved(f("1000"));
}

#[test]
fn quotes_during_an_fx_outage_neither_move_the_mark_nor_close_out_other_positions() {
    let (mut h, a) = (Harness::fx(), acct(1));
    h.deposit(a, "3000");
    h.price(EURUSD, "1");
    h.price(DE40, "20000");
    h.price(BTC, "100000");
    h.market_order(a, DE40, Side::Buy, "1", "20000");
    h.market_order(a, BTC, Side::Buy, "0.1", "100000");
    // EUR-USD goes stale; at its own quote of 17,500, Germany 40 would leave 500 USD of equity
    // against 1,875 of margin and close BTC out.
    h.advance(1_500);
    h.price(BTC, "100000");
    let ev = h.price(DE40, "17500");
    assert!(!ev.iter().any(|e| matches!(e, Event::CloseOut { .. } | Event::Fill(_))), "{ev:?}");
    assert_eq!(h.engine.markets()[&DE40].mark, Some(f("20000")));
    assert_eq!(h.engine.accounts()[&a].positions.len(), 2);
}

#[test]
fn an_fx_price_after_a_market_closes_fills_nothing_there() {
    let (mut h, a) = (Harness::fx(), acct(1));
    h.admin(AdminCommand::UpsertMarket(MarketSpec { schedule: us_equities(), ..de40(500) }));
    h.deposit(a, "100000");
    h.price(EURUSD, "1.08");
    h.price(DE40, "20000");
    h.market_order(a, DE40, Side::Buy, "2", "20000");
    h.order(a, DE40, Side::Sell, "1", OrderKind::Limit { price: f("20050") });
    // Germany 40, on US equity hours here, reaches the limit 100ms before its close, while EUR-USD is
    // stale. EUR-USD prices again 50ms after the close, before any tick: Germany 40 is shut.
    h.advance_to(ny(2026, 10, 20, 16, 0) - 100);
    h.price(DE40, "20100");
    h.advance(150);
    let ev = h.price(EURUSD, "1.08");
    assert!(ev.contains(&Event::MarketClosed { market: DE40 }) && fills(&ev).is_empty(), "{ev:?}");
    assert_eq!(h.engine.accounts()[&a].orders.len(), 1);
}

#[test]
fn a_closed_fx_market_has_no_price_to_convert_at() {
    let (mut h, a) = (Harness::fx(), acct(1));
    h.deposit(a, "100000");
    // EUR-USD keeps US equity hours here, so it shuts at 16:00 New York.
    h.admin(AdminCommand::UpsertMarket(market(EURUSD, "EUR-USD", AssetClass::Fx, us_equities(), 200)));
    h.price(EURUSD, "1.08");
    h.price(DE40, "20000");
    assert_eq!(fills(&h.market_order(a, DE40, Side::Buy, "1", "20000")).len(), 1);
    // Quoted 100ms before the close, with no tick or EUR-USD price after it, EUR-USD still has no
    // price 50ms after the close.
    h.advance_to(ny(2026, 10, 20, 16, 0) - 100);
    h.price(EURUSD, "1.08");
    h.price(DE40, "20000");
    h.advance(150);
    let ev = h.market_order(a, DE40, Side::Sell, "1", "20000");
    assert_eq!(rejection(&ev), &Reject::NoFxPrice { fx_market: EURUSD });
    h.advance_to(ny(2026, 10, 20, 16, 30));
    h.price(EURUSD, "1.08");
    h.price(DE40, "20000");
    let ev = h.market_order(a, DE40, Side::Sell, "1", "20000");
    assert_eq!(rejection(&ev), &Reject::NoFxPrice { fx_market: EURUSD });
}

#[test]
fn close_out_passes_over_a_market_whose_fx_market_has_no_price() {
    let (mut h, a) = (Harness::fx(), acct(1));
    h.deposit(a, "3000");
    h.price(EURUSD, "1");
    h.price(DE40, "20000");
    h.price(BTC, "100000");
    h.market_order(a, DE40, Side::Buy, "1", "20000");
    h.market_order(a, BTC, Side::Buy, "0.1", "100000");
    h.price(DE40, "18500");
    // EUR-USD goes stale. BTC's drop takes the account under the close-out level: Germany 40 is the
    // larger loser but cannot trade, so BTC closes.
    h.advance(1_500);
    h.price(DE40, "18500");
    let ev = h.price(BTC, "92000");
    assert!(ev.iter().any(|e| matches!(e, Event::CloseOut { .. })), "{ev:?}");
    let fill = only_fill(&ev);
    assert!(fill.liquidation && fill.market == BTC, "{fill:?}");
    assert!(h.engine.accounts()[&a].positions.contains_key(&DE40));
}

#[test]
fn an_fx_price_sets_every_mark_it_moves_before_any_market_acts_on_one() {
    // Two euro markets, one losing and one gaining during an EUR-USD outage, with a resting buy in
    // the loser. Germany 40 is dealt before France 40 (market id order); its fill's margin check
    // reads France 40's stored mark, so the order is filled only if France 40 is re-marked first:
    // at the stale mark the account shows 1,000 USD against a 1,300 requirement and the order is
    // cancelled instead.
    let (mut h, a) = (Harness::fx(), acct(1));
    let fr40 = MarketSpec { id: FR40, symbol: "FR40".into(), ..de40(500) };
    h.admin(AdminCommand::UpsertMarket(fr40));
    h.deposit(a, "1000");
    h.price(EURUSD, "1");
    h.price(DE40, "20000");
    h.price(FR40, "8000");
    h.market_order(a, FR40, Side::Buy, "1", "8000");
    // A buy limit below Germany 40's last mark rests until the outage drops the market through it.
    h.order(a, DE40, Side::Buy, "1", OrderKind::Limit { price: f("19000") });
    h.advance(1_500);
    h.price(DE40, "18000");
    h.price(FR40, "9000");
    let ev = h.price(EURUSD, "1");
    let fill = only_fill(&ev);
    assert!(fill.market == DE40 && fill.side == Side::Buy && fill.qty == f("1") && !fill.liquidation, "{fill:?}");
    assert!(!ev.iter().any(|e| matches!(e, Event::CloseOut { .. } | Event::OrderCancelled { .. })), "{ev:?}");
    assert_eq!((h.engine.markets()[&DE40].mark, h.engine.markets()[&FR40].mark), (Some(f("18000")), Some(f("9000"))));

    // EUR-USD itself and Germany 40: EUR-USD returns 2% lower after Germany 40 rallied 5% in the
    // outage. With Germany 40 at its fresh price the account is margined; at its old one it is not.
    let (mut h, a) = (Harness::fx(), acct(1));
    h.deposit(a, "3000");
    h.price(EURUSD, "1");
    h.price(DE40, "20000");
    h.market_order(a, EURUSD, Side::Buy, "100000", "1");
    h.market_order(a, DE40, Side::Buy, "1", "20000");
    h.advance(1_500);
    h.price(DE40, "21000");
    let ev = h.price(EURUSD, "0.98");
    assert!(!ev.iter().any(|e| matches!(e, Event::CloseOut { .. })), "{ev:?}");
}

#[test]
fn an_fx_listing_change_is_a_price_update_for_the_markets_that_convert_at_it() {
    let (mut h, a) = (Harness::fx(), acct(1));
    h.admin(AdminCommand::UpsertMarket(de40(100)));
    h.deposit(a, "2300");
    h.price(EURUSD, "1.1");
    h.price(DE40, "20000");
    h.market_order(a, DE40, Side::Buy, "10", "20000");
    h.price(DE40, "19900");
    // EUR-USD goes stale, and only one venue, one short of its minimum, quotes 1.20: the account
    // keeps its 1.10 valuation, 1,200 of equity against a 1,094.5 close-out level.
    h.advance(1_500);
    h.quotes(EURUSD, &["1.2"]);
    let ev = h.price(DE40, "19900");
    assert!(!ev.iter().any(|e| matches!(e, Event::CloseOut { .. })), "{ev:?}");
    // Relisted to need one source, EUR-USD has a price at 1.20 from that command: Germany 40 marks
    // at it and its holder, at 1,100 against a 1,194 level, is closed out.
    let base = market(EURUSD, "EUR-USD", AssetClass::Fx, Schedule::AlwaysOpen, 200);
    let one_source = MarketSpec { price_rules: PriceRules { min_sources: 1, ..base.price_rules }, ..base };
    let ev = h.cmd(Command::Admin(AdminCommand::UpsertMarket(one_source)));
    assert!(ev.iter().any(|e| matches!(e, Event::CloseOut { .. })), "{ev:?}");
    let fill = only_fill(&ev);
    assert!(fill.liquidation && fill.market == DE40 && fill.fx_index == Some(f("1.2")), "{fill:?}");
}

#[test]
fn an_fx_move_closes_out_through_a_market_that_cannot_be_marked() {
    // Germany 40 keeps US equity hours here and has closed; its profit in euros carries the account
    // until EUR-USD falls, and BTC, which can trade, is closed on that EUR-USD price.
    let (mut h, a) = (Harness::fx(), acct(1));
    h.admin(AdminCommand::UpsertMarket(MarketSpec { schedule: us_equities(), ..de40(500) }));
    h.deposit(a, "2000");
    h.price(EURUSD, "1");
    h.price(DE40, "20000");
    h.price(BTC, "100000");
    h.market_order(a, DE40, Side::Buy, "1", "20000");
    h.market_order(a, BTC, Side::Buy, "0.1", "100000");
    h.price(DE40, "24000");
    h.advance_to(ny(2026, 10, 20, 16, 30));
    h.price(BTC, "70000");
    let ev = h.price(EURUSD, "0.39");
    assert!(ev.iter().any(|e| matches!(e, Event::CloseOut { .. })), "{ev:?}");
    let fill = only_fill(&ev);
    assert!(fill.liquidation && fill.market == BTC, "{fill:?}");
}
