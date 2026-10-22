//! The session's `V12` part: every command first brings each market to its sequenced time.

use engine_core::command::{AdminCommand, Command, OrderKind};
use engine_core::market::{MarketSpec, QuoteCurrency};
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::types::{AssetClass, MarketId, Side};

use crate::common::{AAPL, BTC, DE40, ETH, EURUSD, Harness, f, market, ny, quote, us_equities};

/// A second US equity, listed after AAPL so it is processed after it.
const MSFT: MarketId = 16;
/// A USD-JPY listing made under `V12`, quoted in yen at its own index.
const USDJPY_V12: MarketId = 17;
/// Japan 225, quoted in yen at `USDJPY_V12`.
const JP225: MarketId = 18;

/// Activates `V12` at the next seq. A buy limit placed after Monday's close fills at Tuesday's
/// gapped open on the tick that opens AAPL, before a market order just after it. At Wednesday's
/// open, where AAPL and MSFT longs both gap down, a stop closes the MSFT long before close-outs, which
/// see both opening marks. An ETH venue's quote ages out without moving the index on that venue's
/// own refresh, then does move it to a sell limit, which the next command fills. A price just after
/// Wednesday's close closes both equities first, and a BTC venue whose stamps lead by 300ms prices
/// the market at once. An EUR-USD venue ages out, and the next command moves Germany 40's FX mark
/// with the index. Last, a USD-JPY listed now takes its FX mark from its first price, and it and
/// Japan 225, quoted in yen at it, trade at once.
pub(crate) fn session(h: &mut Harness) {
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V12 });
    let [q, r] = [29, 30].map(|seed| h.signer(seed));
    h.deposit(q, "10000");
    h.deposit(r, "2400");
    h.operator_admin(AdminCommand::UpsertMarket(market(MSFT, "MSFT", AssetClass::Equity, us_equities(), 2_000)));
    h.order(q, AAPL, Side::Buy, "1", OrderKind::Limit { price: f("196") });
    h.advance_to(ny(2026, 10, 27, 9, 30) - 100);
    h.price(AAPL, "195");
    h.advance(100);
    h.cmd(Command::Tick);
    h.advance(100);
    h.market_order(q, AAPL, Side::Sell, "1", "195");

    // Longs in AAPL and MSFT that use all of `r`'s margin, and a stop on the MSFT long. With MSFT
    // still marked at 900 at Wednesday's open, AAPL's loss would put `r` below its close-out level;
    // with both opening marks, the stop closes MSFT at 850 first and `r` stays above it.
    h.price(MSFT, "1000");
    h.market_order(r, MSFT, Side::Buy, "10", "1000");
    h.market_order(r, AAPL, Side::Buy, "10", "195");
    h.order(r, MSFT, Side::Sell, "10", OrderKind::Stop { price: f("880") });
    h.advance(61_000);
    h.price(MSFT, "900");
    h.advance_to(ny(2026, 10, 28, 9, 30) - 100);
    h.price(AAPL, "150");
    h.price(MSFT, "850");
    h.advance(100);
    h.cmd(Command::Tick);

    // With ETH's stress floor lifted, the lowest of three ETH venues ages out on the command that
    // refreshes it, which counts its new quote first and keeps the index; when it next ages out, the
    // index moves to a sell limit, which the next command fills, whatever it is.
    h.operator_admin(AdminCommand::SetStressFloor { market: Some(ETH), level: 0 });
    h.quotes(ETH, &["2999.4", "3000", "3000.6"]);
    h.order(q, ETH, Side::Sell, "1", OrderKind::Limit { price: f("3000.2") });
    let upper_venues = |ts| vec![quote("venue1", "3000", ts), quote("venue2", "3000.6", ts)];
    h.advance(600);
    h.cmd(Command::Prices { market: ETH, quotes: upper_venues(h.ts) });
    h.advance(401);
    h.cmd(Command::Prices { market: ETH, quotes: vec![quote("venue0", "2999.4", h.ts - 1)] });
    h.advance(599);
    h.cmd(Command::Prices { market: ETH, quotes: upper_venues(h.ts) });
    h.advance(401);
    h.cmd(Command::Deposit { account: r, amount: f("1"), chain_ref: "0xfeed:4".into() });

    // Just after Wednesday's close, a BTC price closes both equities before it applies. Its second
    // venue stamps every quote 300ms ahead, and still prices the market at once.
    h.advance_to(ny(2026, 10, 28, 16, 0) + 50);
    for _ in 0..2 {
        h.cmd(Command::Prices {
            market: BTC,
            quotes: vec![quote("venue0", "80000", h.ts), quote("venue1", "80010", h.ts + 300)],
        });
        h.advance(100);
    }
    h.market_order(q, BTC, Side::Buy, "0.01", "80005");

    // The lowest of three EUR-USD venues ages out, and a deposit, the next command, moves Germany
    // 40's FX mark to the new index.
    h.quotes(EURUSD, &["1.0996", "1.1", "1.1004"]);
    h.price(DE40, "20000");
    h.advance(600);
    h.cmd(Command::Prices {
        market: EURUSD,
        quotes: vec![quote("venue1", "1.1", h.ts), quote("venue2", "1.1004", h.ts)],
    });
    h.price(DE40, "20000");
    h.advance(401);
    h.cmd(Command::Deposit { account: q, amount: f("1"), chain_ref: "0xfeed:5".into() });

    let yen = QuoteCurrency::Jpy { fx_market: USDJPY_V12 };
    let usdjpy = market(USDJPY_V12, "USD-JPY", AssetClass::Fx, Schedule::AlwaysOpen, 200);
    h.operator_admin(AdminCommand::UpsertMarket(MarketSpec { quote_currency: yen, ..usdjpy }));
    let jp225 = market(JP225, "JP225", AssetClass::Index, Schedule::AlwaysOpen, 500);
    h.operator_admin(AdminCommand::UpsertMarket(MarketSpec { quote_currency: yen, ..jp225 }));
    h.price(USDJPY_V12, "150");
    h.market_order(q, USDJPY_V12, Side::Buy, "1000", "150");
    h.price(JP225, "40000");
    h.market_order(q, JP225, Side::Buy, "0.1", "40000");
}
