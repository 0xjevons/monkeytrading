//! Quote timing: from `V12`, every command first processes any index that changed without a price
//! update, and a venue quote stamped ahead counts at once, end to end through `Engine::apply`.

mod common;

use common::{
    AAPL, BTC, Client, DE40, EURUSD, Harness, USDJPY, acct, de40, f, fills, market, ny, operator, params, quote,
    rejection, start, us_equities,
};
use engine_core::auth::ClientAuth;
use engine_core::command::{AdminCommand, Command, OrderKind, Sequenced};
use engine_core::event::{Event, Reject};
use engine_core::market::{MarketSpec, QuoteCurrency};
use engine_core::price::{MAX_QUOTE_LEAD_MS, PriceError};
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::types::{AccountId, AssetClass, MarketId, Side};
use engine_core::{Engine, Fixed};

/// A second US equity, listed after AAPL so it is processed after it.
const MSFT: MarketId = 7;
/// Japan 225, quoted in yen at USD-JPY.
const JP225: MarketId = 8;

/// Leaves acct(1) with a resting order that a changed index reaches, and returns the command that
/// must process that index and the index.
type Setup = fn() -> (Harness, Command, &'static str);

#[test]
fn a_gapped_open_fills_the_stops_it_gapped_through_at_the_tick_that_opens_it() {
    let (mut h, a, b) = gapped_open(Harness::v12());
    let ev = h.cmd(Command::Tick);
    // The open, then the stop at the first index after it.
    assert_eq!(ev[0], Event::MarketOpened { market: AAPL });
    assert_eq!(fills_by(&ev), [(a, f("180"))], "{ev:?}");
    assert_eq!(h.engine.markets()[&AAPL].mark, Some(f("180")));

    // A market order just after the open fills against the same index, after the stop.
    h.advance(100);
    let ev = h.market_order(b, AAPL, Side::Buy, "1", "180");
    assert_eq!(fills_by(&ev), [(b, f("180"))]);
    assert!(h.engine.accounts()[&a].orders.is_empty() && h.engine.accounts()[&a].positions.is_empty());
}

#[test]
fn the_first_order_after_an_open_fills_after_the_stops_it_gapped_through() {
    let (mut h, a, b) = gapped_open(Harness::v12());
    h.advance(100);
    let ev = h.market_order(b, AAPL, Side::Buy, "1", "180");
    assert_eq!(ev[0], Event::MarketOpened { market: AAPL });
    assert_eq!(fills_by(&ev), [(a, f("180")), (b, f("180"))], "{ev:?}");
}

#[test]
fn before_v12_a_gapped_open_leaves_its_stops_for_the_next_price() {
    let (mut h, a, b) = gapped_open(Harness::v11());
    assert_eq!(h.cmd(Command::Tick), [Event::MarketOpened { market: AAPL }]);
    h.advance(100);
    let ev = h.market_order(b, AAPL, Side::Buy, "1", "180");
    assert_eq!(fills_by(&ev), [(b, f("180"))]);
    assert_eq!(h.engine.accounts()[&a].orders.len(), 1);
}

#[test]
fn an_index_changed_without_a_price_update_is_processed_before_the_command_acts() {
    let cases: [(&str, Setup); 5] = [
        ("the market opens", market_opens),
        ("a quote ages out", quote_ages_out),
        ("the price rules change", price_rules_change),
        ("the quote currency changes", quote_currency_changes),
        ("a quote stamped ahead before V12 is reached", quote_stamped_ahead_before_v12_is_reached),
    ];
    let failures: Vec<String> = cases
        .into_iter()
        .filter_map(|(case, setup)| {
            let (mut h, cmd, index) = setup();
            let ev = h.cmd(cmd);
            // acct(1)'s resting order fills at the new index, which marks its market, before the
            // command's own outcome.
            let marked = fills(&ev).first().map(|fill| h.engine.markets()[&fill.market].mark);
            let processed = fills_by(&ev) == [(acct(1), f(index))]
                && marked == Some(Some(f(index)))
                && matches!(ev.last(), Some(Event::Deposited { .. } | Event::AdminApplied));
            (!processed).then(|| format!("{case}: {ev:?}"))
        })
        .collect();
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn a_price_command_counts_its_own_quotes_before_its_market_is_processed() {
    // BTC's lowest venue has just aged out, leaving 100,020, which reaches a sell limit at 100,010;
    // the command that finds it is that venue's own refresh, which keeps the index at 100,000.
    let (mut h, _, _) = quote_ages_out();
    let ev = h.cmd(Command::Prices { market: BTC, quotes: vec![quote("venue0", "99960", h.ts - 1)] });
    assert!(fills(&ev).is_empty(), "{ev:?}");
    assert_eq!(h.engine.markets()[&BTC].mark, Some(f("100000")));
    assert_eq!(h.engine.accounts()[&acct(1)].orders.len(), 1);
}

#[test]
fn a_price_update_that_leaves_the_index_unchanged_still_processes_its_market() {
    let mut h = Harness::v12();
    let a = acct(1);
    h.deposit(a, "100000");
    h.price(BTC, "100000");
    // At stress level 1 the 5bps spread prices a buy limit at the mid above its limit, so it rests.
    h.operator_admin(AdminCommand::SetStressFloor { market: Some(BTC), level: 1 });
    let ev = h.order(a, BTC, Side::Buy, "0.1", OrderKind::Limit { price: f("100000") });
    assert!(fills(&ev).is_empty(), "{ev:?}");
    h.operator_admin(AdminCommand::SetStressFloor { market: Some(BTC), level: 0 });

    // The next price repeats the index, and still triggers the limit, now inside it.
    h.advance(100);
    let ev = h.price(BTC, "100000");
    assert_eq!(fills_by(&ev), [(a, f("100000"))], "{ev:?}");
}

#[test]
fn a_rollover_acts_after_the_index_time_alone_changed_is_processed() {
    let mut h = Harness::v12();
    let c = acct(1);
    h.deposit(c, "10560");
    // A long bought at 105,555 with all of the account's margin, then marked down to 100,000 a
    // minute later, 5 above its close-out level, with a stop at 99,980.
    let rollover = ny(2026, 10, 20, 17, 0);
    h.advance_to(rollover - 62_000);
    h.price(BTC, "105555");
    h.market_order(c, BTC, Side::Buy, "1", "105555");
    h.order(c, BTC, Side::Sell, "1", OrderKind::Stop { price: f("99980") });
    h.advance_to(rollover - 1_000);
    h.quotes(BTC, &["99960", "100000", "100040"]);
    h.advance(500);
    h.cmd(Command::Prices {
        market: BTC,
        quotes: vec![quote("venue0", "99960", h.ts), quote("venue1", "100000", h.ts)],
    });

    // Just after the rollover the highest venue ages out, moving the index to 99,980. The stop
    // closes the long there before the rollover charges financing or checks close-outs, so neither
    // touches the account.
    h.advance(501);
    let ev = h.cmd(Command::Tick);
    assert_eq!(fills(&ev).iter().map(|fill| (fill.price, fill.liquidation)).collect::<Vec<_>>(), [(f("99980"), false)]);
    assert!(!ev.iter().any(|e| matches!(e, Event::CloseOut { .. } | Event::Financing { .. })), "{ev:?}");
}

#[test]
fn the_first_parameters_process_every_market_priced_before_them() {
    let mut engine = Engine::new();
    let btc = market(BTC, "BTC-USD", AssetClass::Crypto, Schedule::AlwaysOpen, 1_000);
    let prices = vec![quote("venue0", "100000", start()), quote("venue1", "100000", start())];
    let cmds = [
        Command::Admin(AdminCommand::SetClientAuth(ClientAuth::Unsigned)),
        Command::Admin(AdminCommand::SetOperator(operator())),
        Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V12 }),
        Command::Admin(AdminCommand::UpsertMarket(btc)),
        Command::Prices { market: BTC, quotes: prices },
        Command::Admin(AdminCommand::SetParams(params())),
    ];
    let marks: Vec<Option<Fixed>> = (0..)
        .zip(cmds)
        .map(|(seq, cmd)| {
            engine.apply(&Sequenced { seq, ts: start(), cmd }).expect("in sequence");
            engine.markets().get(&BTC).and_then(|market| market.mark)
        })
        .collect();
    // Unpriced until the parameters arrive, and marked by the command that sets them.
    assert_eq!(marks, [None, None, None, None, None, Some(f("100000"))]);
    assert_eq!(engine.markets()[&BTC].processed_index.map(|index| index.mid), Some(f("100000")));
}

#[test]
fn an_fx_index_changed_by_time_alone_re_marks_the_markets_that_convert_at_it() {
    for (version, mut h) in [(ProtocolVersion::V11, Harness::v11()), (ProtocolVersion::V12, Harness::v12())] {
        h.list_fx();
        // Three EUR-USD venues give 1.08, at which Germany 40's euros convert.
        h.quotes(EURUSD, &["1.0796", "1.08", "1.0804"]);
        h.price(DE40, "20000");
        h.advance(600);
        let upper_venues = vec![quote("venue1", "1.08", h.ts), quote("venue2", "1.0804", h.ts)];
        h.cmd(Command::Prices { market: EURUSD, quotes: upper_venues });
        h.price(DE40, "20000");

        // The lowest EUR-USD venue ages out, moving the index to 1.0802, and a deposit is the next
        // command.
        h.advance(401);
        h.cmd(Command::Deposit { account: acct(2), amount: f("1"), chain_ref: "0xnext".into() });
        let fx_mark = h.engine.markets()[&DE40].fx_mark;
        let expected = if version >= ProtocolVersion::V12 { "1.0802" } else { "1.08" };
        assert_eq!(fx_mark, Some(f(expected)), "{version:?}");
    }
}

#[test]
fn a_market_that_converts_at_its_own_index_prices_from_its_first_quotes() {
    let mut h = Harness::v12();
    h.list_fx();
    let jp225 = market(JP225, "JP225", AssetClass::Index, Schedule::AlwaysOpen, 500);
    let yen = MarketSpec { quote_currency: QuoteCurrency::Jpy { fx_market: USDJPY }, ..jp225 };
    h.operator_admin(AdminCommand::UpsertMarket(yen));
    let a = acct(1);
    h.deposit(a, "100000");

    // USD-JPY, quoted in yen at its own index, takes its FX mark from its first price, and both it
    // and Japan 225 trade at once.
    h.price(USDJPY, "150");
    assert_eq!(h.engine.markets()[&USDJPY].fx_mark, Some(f("150")));
    let ev = h.market_order(a, USDJPY, Side::Buy, "1000", "150");
    assert_eq!(fills(&ev).iter().map(|fill| fill.fx_index).collect::<Vec<_>>(), [Some(f("150"))], "{ev:?}");
    h.price(JP225, "40000");
    let ev = h.market_order(a, JP225, Side::Buy, "0.1", "40000");
    let filled: Vec<_> = fills(&ev).iter().map(|fill| (fill.price, fill.fx_index)).collect();
    assert_eq!(filled, [(f("40000"), Some(f("150")))], "{ev:?}");
}

#[test]
fn a_market_relisted_to_convert_at_its_own_index_takes_its_fx_mark_at_once() {
    let mut h = Harness::v12();
    h.list_fx();
    h.price(USDJPY, "150");
    // Listed as another market in USD, it drops its FX mark; listed again as USD-JPY in yen, its
    // own index, unchanged, gives it the mark back on that command.
    let usdjpy = market(USDJPY, "USD-JPY", AssetClass::Fx, Schedule::AlwaysOpen, 200);
    let renamed = MarketSpec { symbol: "USDJPY-OLD".into(), ..usdjpy.clone() };
    h.operator_admin(AdminCommand::UpsertMarket(renamed));
    assert_eq!(h.engine.markets()[&USDJPY].fx_mark, None);
    let yen = MarketSpec { quote_currency: QuoteCurrency::Jpy { fx_market: USDJPY }, ..usdjpy };
    h.operator_admin(AdminCommand::UpsertMarket(yen));
    assert_eq!(h.engine.markets()[&USDJPY].fx_mark, Some(f("150")));
}

#[test]
fn a_shared_open_fills_resting_orders_in_every_market_before_any_close_out() {
    let mut h = Harness::v12();
    let c = acct(1);
    h.operator_admin(AdminCommand::UpsertMarket(market(MSFT, "MSFT", AssetClass::Equity, us_equities(), 2_000)));
    h.deposit(c, "2400");
    h.price(MSFT, "1000");
    h.price(AAPL, "200");
    // 2,000 + 400 of margin, all of the account's equity, and a stop on the MSFT long.
    h.market_order(c, MSFT, Side::Buy, "10", "1000");
    h.market_order(c, AAPL, Side::Buy, "10", "200");
    h.order(c, MSFT, Side::Sell, "10", OrderKind::Stop { price: f("880") });
    h.advance(61_000);
    h.price(MSFT, "900");
    h.advance_to(ny(2026, 10, 20, 16, 30));
    h.cmd(Command::Tick);

    // Both open gapped down. With MSFT still marked at 900, AAPL's loss alone would put the account
    // below its close-out level; with every mark at the open, the stop closes MSFT at 850 first and
    // the account stays above it.
    h.advance_to(ny(2026, 10, 21, 9, 30) - 100);
    h.price(AAPL, "150");
    h.price(MSFT, "850");
    h.advance(100);
    let ev = h.cmd(Command::Tick);
    assert!(!ev.iter().any(|e| matches!(e, Event::CloseOut { .. })), "{ev:?}");
    let filled: Vec<_> = fills(&ev).iter().map(|fill| (fill.market, fill.price, fill.liquidation)).collect();
    assert_eq!(filled, [(MSFT, f("850"), false)]);
    assert_eq!(h.engine.accounts()[&c].positions[&AAPL].qty, f("10"));
}

#[test]
fn a_price_just_after_a_close_closes_every_market_past_its_session_before_it_applies() {
    let mut h = Harness::v12();
    let c = acct(1);
    h.deposit(c, "4000");
    h.price(AAPL, "200");
    h.price(BTC, "100000");
    h.market_order(c, AAPL, Side::Buy, "50", "200");
    h.market_order(c, BTC, Side::Buy, "0.05", "100000");
    h.advance_to(ny(2026, 10, 20, 16, 0) - 100);
    h.price(AAPL, "160");

    // 50ms after AAPL's close, before any tick, BTC falls and puts the account below its close-out
    // level. AAPL is the larger loser, but the price closes its session first.
    h.advance_to(ny(2026, 10, 20, 16, 0) + 50);
    let ev = h.price(BTC, "70000");
    assert_eq!(ev[0], Event::MarketClosed { market: AAPL });
    assert_eq!(h.engine.markets()[&AAPL].processed_index, None);
    let liquidated: Vec<MarketId> = fills(&ev).iter().filter(|fill| fill.liquidation).map(|fill| fill.market).collect();
    assert_eq!(liquidated, [BTC], "{ev:?}");
    assert_eq!(h.engine.accounts()[&c].positions[&AAPL].qty, f("50"));
}

#[test]
fn a_venue_whose_stamps_steadily_lead_counts_toward_the_index() {
    for (version, mut h) in [(ProtocolVersion::V11, Harness::v11()), (ProtocolVersion::V12, Harness::v12())] {
        let a = acct(1);
        h.deposit(a, "100000");
        // venue0 stamps at the sequenced time; venue1 quotes every 100ms, each stamped 300ms ahead.
        let marks: Vec<_> = (0..10)
            .map(|_| {
                h.advance(100);
                let quotes = vec![quote("venue0", "100000", h.ts), quote("venue1", "100100", h.ts + 300)];
                h.cmd(Command::Prices { market: BTC, quotes });
                h.engine.markets()[&BTC].mark
            })
            .collect();
        let ev = h.market_order(a, BTC, Side::Buy, "0.1", "100050");
        if version >= ProtocolVersion::V12 {
            assert_eq!(marks, [Some(f("100050")); 10]);
            assert_eq!(fills(&ev)[0].price, f("100050"));
        } else {
            assert_eq!(marks, [None; 10]);
            let stale = PriceError::Stale { fresh: 1, needed: 2 };
            assert_eq!(rejection(&ev), &Reject::NoPrice { error: stale }, "{ev:?}");
        }
    }
}

#[test]
fn a_quote_stamped_ahead_counts_from_the_sequenced_time_it_arrives_at() {
    for lead_ms in [1, 300, MAX_QUOTE_LEAD_MS] {
        let mut h = Harness::v12();
        let arrived = h.ts;
        let quotes = vec![quote("venue0", "100000", arrived), quote("venue1", "100100", arrived + lead_ms)];
        h.cmd(Command::Prices { market: BTC, quotes });
        assert_eq!(h.engine.markets()[&BTC].quotes["venue1"].ts, arrived, "{lead_ms}ms");
        assert_eq!(h.engine.markets()[&BTC].mark, Some(f("100050")), "{lead_ms}ms");

        // It ages from its arrival: past the 1s maximum age it no longer counts, whatever its stamp.
        h.advance(1_000);
        h.cmd(Command::Prices { market: BTC, quotes: vec![quote("venue0", "100000", h.ts)] });
        assert_eq!(h.engine.markets()[&BTC].processed_index.map(|index| index.sources), Some(2), "{lead_ms}ms");
        h.advance(1);
        h.cmd(Command::Prices { market: BTC, quotes: vec![quote("venue0", "100000", h.ts)] });
        assert_eq!(h.engine.markets()[&BTC].processed_index, None, "{lead_ms}ms");
    }
}

#[test]
fn a_venue_quote_stamped_before_its_anchored_one_does_not_replace_it() {
    let mut h = Harness::v12();
    let arrived = h.ts;
    h.cmd(Command::Prices { market: BTC, quotes: vec![quote("venue0", "100000", arrived + 500)] });
    // 100ms later the venue's clock steps back: a quote stamped before the sequenced time of the
    // first one is older than it and is dropped; one stamped after it replaces it.
    h.advance(100);
    h.cmd(Command::Prices { market: BTC, quotes: vec![quote("venue0", "99000", arrived - 1)] });
    assert_eq!(h.engine.markets()[&BTC].quotes["venue0"].mid, f("100000"));
    h.cmd(Command::Prices { market: BTC, quotes: vec![quote("venue0", "99000", arrived + 1)] });
    assert_eq!(h.engine.markets()[&BTC].quotes["venue0"].mid, f("99000"));
}

// ── Helpers ──

/// AAPL under `h`'s rules: acct(1) is long 10 at 200 with a sell stop at 190 resting over Tuesday's
/// close, acct(2) is funded, and Wednesday's quotes at 180 arrived 100ms before the open, which is
/// now.
fn gapped_open(mut h: Harness) -> (Harness, AccountId, AccountId) {
    let (a, b) = (acct(1), acct(2));
    h.deposit(a, "100000");
    h.deposit(b, "100000");
    h.price(AAPL, "200");
    h.market_order(a, AAPL, Side::Buy, "10", "200");
    h.order(a, AAPL, Side::Sell, "10", OrderKind::Stop { price: f("190") });
    h.advance_to(ny(2026, 10, 20, 16, 30));
    h.cmd(Command::Tick);
    h.advance_to(ny(2026, 10, 21, 9, 30) - 100);
    h.price(AAPL, "180");
    h.advance(100);
    (h, a, b)
}

/// A sell limit at 205 rests over AAPL's close, quotes at 210 arrived before the open, which is now,
/// and a deposit is the first command after it.
fn market_opens() -> (Harness, Command, &'static str) {
    let mut h = Harness::v12();
    h.deposit(acct(1), "100000");
    h.price(AAPL, "200");
    h.order(acct(1), AAPL, Side::Sell, "1", OrderKind::Limit { price: f("205") });
    h.advance_to(ny(2026, 10, 20, 16, 30));
    h.cmd(Command::Tick);
    h.advance_to(ny(2026, 10, 21, 9, 30) - 100);
    h.price(AAPL, "210");
    h.advance(100);
    (h, deposit(), "210")
}

/// Three venues price BTC at 100,000, a sell limit at 100,010 rests, the lowest venue's quote has
/// just aged out, leaving 100,020, and a deposit is the next command.
fn quote_ages_out() -> (Harness, Command, &'static str) {
    let mut h = Harness::v12();
    h.deposit(acct(1), "100000");
    h.quotes(BTC, &["99960", "100000", "100040"]);
    h.order(acct(1), BTC, Side::Sell, "0.1", OrderKind::Limit { price: f("100010") });
    h.advance(600);
    let quotes = vec![quote("venue1", "100000", h.ts), quote("venue2", "100040", h.ts)];
    h.cmd(Command::Prices { market: BTC, quotes });
    assert_eq!(h.engine.markets()[&BTC].mark, Some(f("100000")));
    h.advance(401);
    (h, deposit(), "100020")
}

/// One venue quotes BTC, too few for an index, a sell limit at 99,990 rests, and the next command is
/// a listing change that lets one venue price it.
fn price_rules_change() -> (Harness, Command, &'static str) {
    let mut h = Harness::v12();
    h.deposit(acct(1), "100000");
    h.quotes(BTC, &["100000"]);
    h.order(acct(1), BTC, Side::Sell, "0.1", OrderKind::Limit { price: f("99990") });
    let mut btc = market(BTC, "BTC-USD", AssetClass::Crypto, Schedule::AlwaysOpen, 1_000);
    btc.price_rules.min_sources = 1;
    let listing = signed(&h, AdminCommand::UpsertMarket(btc));
    (h, listing, "100000")
}

/// Germany 40 is quoted in euros at an EUR-USD market with no price, so its quotes at 20,000 give it
/// no price and a sell limit at 19,990 rests; the next command lists it in USD.
fn quote_currency_changes() -> (Harness, Command, &'static str) {
    let mut h = Harness::v12();
    h.list_fx();
    h.deposit(acct(1), "100000");
    h.price(DE40, "20000");
    h.order(acct(1), DE40, Side::Sell, "1", OrderKind::Limit { price: f("19990") });
    let in_dollars = MarketSpec { quote_currency: QuoteCurrency::Usd, ..de40(500) };
    let listing = signed(&h, AdminCommand::UpsertMarket(in_dollars));
    (h, listing, "20000")
}

/// Under `V11`, a sell limit at 100,040 rests and venue1's next quote, at 100,100, is stamped 500ms
/// ahead, so BTC has no index until it is reached. `V12` activates, a deposit 100ms later changes
/// nothing, and now the quote is reached and a deposit is the next command.
fn quote_stamped_ahead_before_v12_is_reached() -> (Harness, Command, &'static str) {
    let mut h = Harness::v11();
    h.deposit(acct(1), "100000");
    h.price(BTC, "100000");
    h.order(acct(1), BTC, Side::Sell, "0.1", OrderKind::Limit { price: f("100040") });
    h.cmd(Command::Prices { market: BTC, quotes: vec![quote("venue1", "100100", h.ts + 500)] });
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V12 });
    h.advance(100);
    h.deposit(acct(1), "1");
    h.advance(400);
    (h, deposit(), "100050")
}

/// `command` signed by the harness operator with its next nonce.
fn signed(h: &Harness, command: AdminCommand) -> Command {
    let operator = Client { expires_at: h.ts + 60_000, ..Client::operator() };
    operator.admin(h.engine.next_request_nonce(operator.account), command)
}

/// A deposit to acct(2), a command that carries neither prices nor an order.
fn deposit() -> Command {
    Command::Deposit { account: acct(2), amount: f("1"), chain_ref: "0xnext".into() }
}

/// Who filled at what price, in event order.
fn fills_by(events: &[Event]) -> Vec<(AccountId, Fixed)> {
    fills(events).iter().map(|fill| (fill.account, fill.price)).collect()
}
