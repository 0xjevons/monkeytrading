//! USD settlement: markets quoted in another currency accrue PnL in it and convert to USD at their
//! FX market's index, for marks, fills, margin, house exposure, and financing, and deal only while
//! that FX market has a price.

mod common;

use common::events::{fills, only_fill, rejection};
use common::{BTC, DE40, EURUSD, Harness, UNLISTED, USDJPY, acct, de40, f, market, ny};
use engine_core::Fixed;
use engine_core::auth::ClientAuth;
use engine_core::command::{AdminCommand, Command};
use engine_core::event::{Event, Reject};
use engine_core::market::{MarketSpec, QuoteCurrency, Tier};
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::types::{AssetClass, MarketId, Side};

/// A GBP-USD market some tests list.
const GBPUSD: MarketId = 7;

#[test]
fn usd_pnl_and_margin_follow_the_fx_index_while_the_market_holds_still() {
    let (mut h, a) = (Harness::fx(), acct(1));
    h.deposit(a, "100000");
    h.price(EURUSD, "1.08");
    h.price(DE40, "20000");
    let fill = only_fill(&h.market_order(a, DE40, Side::Buy, "2", "20000")).clone();
    assert_eq!((fill.price, fill.realised_pnl, fill.fx_index), (f("20000"), Fixed::ZERO, Some(f("1.08"))));
    h.price(DE40, "20100");
    // 200 EUR of PnL and 5% margin on 40,200 EUR of notional, at 1.08.
    assert_eq!(h.equity_and_margin(a), (f("100216"), f("2170.8")));
    h.price(EURUSD, "1.1");
    assert_eq!(h.equity_and_margin(a), (f("100220"), f("2211")));
    assert_eq!(h.engine.house_nav(), f("99999780"));
    assert_eq!(h.engine.house_gross_exposure(), f("44220"));
}

#[test]
fn realised_pnl_converts_at_the_fx_index_when_it_is_realised() {
    let (mut h, a) = (Harness::fx(), acct(1));
    h.deposit(a, "100000");
    h.price(EURUSD, "1.08");
    h.price(DE40, "20000");
    h.market_order(a, DE40, Side::Buy, "2", "20000");
    h.price(DE40, "20100");
    h.price(EURUSD, "1.1");
    // 200 EUR of profit realised at 1.10 is 220 USD, less the 10% fee.
    let gain = only_fill(&h.market_order(a, DE40, Side::Sell, "2", "20100")).clone();
    assert_eq!((gain.realised_pnl, gain.profit_fee, gain.fx_index), (f("220"), f("22"), Some(f("1.1"))));
    assert_eq!(h.balance(a), f("100198"));
    // A 100 EUR loss at 1.10 costs 110 USD and pays no fee.
    h.market_order(a, DE40, Side::Buy, "1", "20100");
    h.price(DE40, "20000");
    let loss = only_fill(&h.market_order(a, DE40, Side::Sell, "1", "20000")).clone();
    assert_eq!((loss.realised_pnl, loss.profit_fee), (f("-110"), Fixed::ZERO));
    assert_eq!(h.balance(a), f("100088"));
    h.assert_conserved(Fixed::ZERO);
}

#[test]
fn yen_pnl_converts_by_dividing_by_the_usd_jpy_index() {
    let (mut h, a) = (Harness::fx(), acct(1));
    h.deposit(a, "100000");
    h.price(USDJPY, "150");
    h.market_order(a, USDJPY, Side::Buy, "10000", "150");
    h.price(USDJPY, "151");
    // 10,000 JPY of PnL at 151 yen per USD, rounded down; 2% margin on 1,510,000 JPY, or 10,000 USD.
    assert_eq!(h.equity_and_margin(a), (f("100066.225165562"), f("200")));
    let fill = only_fill(&h.market_order(a, USDJPY, Side::Sell, "10000", "151")).clone();
    assert_eq!((fill.realised_pnl, fill.profit_fee), (f("66.225165562"), f("6.622516557")));
    h.assert_conserved(Fixed::ZERO);
}

#[test]
fn a_flip_is_margined_on_the_pnl_it_realises_in_usd() {
    let (mut h, a) = (Harness::fx(), acct(1));
    h.deposit(a, "1500");
    h.price(USDJPY, "150");
    h.market_order(a, USDJPY, Side::Buy, "10000", "150");
    h.price(USDJPY, "140");
    // Selling 20,000 realises about -100,000 JPY, some -719 USD, and leaves a short needing 200 USD.
    let fill = only_fill(&h.market_order(a, USDJPY, Side::Sell, "20000", "140")).clone();
    assert_eq!(fill.position, f("-10000"));
    assert!(fill.realised_pnl > f("-720") && fill.realised_pnl < f("-719"), "{fill:?}");
}

#[test]
fn margin_and_size_spread_bands_and_the_minimum_notional_are_in_usd() {
    let (mut h, a, b, c) = (Harness::fx(), acct(1), acct(2), acct(3));
    for account in [a, b, c] {
        h.deposit(account, "1000000");
    }
    let mut spec = de40(500);
    spec.margin_tiers = vec![Tier { up_to: Some(f("40000")), bps: 500 }, Tier { up_to: None, bps: 1_000 }];
    spec.min_order_notional = f("50");
    h.admin(AdminCommand::UpsertMarket(spec));
    h.price(EURUSD, "1.08");
    h.price(DE40, "20000");

    // 40,000 EUR is 43,200 USD: 5% on the first 40,000 USD and 10% on the 3,200 above.
    h.market_order(a, DE40, Side::Buy, "2", "20000");
    assert_eq!(h.equity_and_margin(a), (f("1000000"), f("2320")));

    // 0.002 is 40 EUR, or 43.2 USD, under the 50 USD minimum; 0.0024 is 48 EUR but 51.84 USD.
    let small = h.market_order(b, DE40, Side::Buy, "0.002", "20000");
    assert_eq!(rejection(&small), &Reject::InvalidOrder { reason: "below minimum order notional".into() });
    assert_eq!(fills(&h.market_order(b, DE40, Side::Buy, "0.0024", "20000")).len(), 1);

    // 1,000,000 EUR is 1,080,000 USD, 80,000 of it in the 2bps band: 16 USD, or 14.814814815 EUR
    // rounded up, over 50 contracts.
    let large = only_fill(&h.market_order(c, DE40, Side::Buy, "50", "20000")).clone();
    assert_eq!(large.price, f("20000.296296296"));
}

#[test]
fn house_exposure_caps_and_stress_use_usd_notional() {
    let (mut h, a) = (Harness::fx(), acct(1));
    h.deposit(a, "10000000");
    h.price(EURUSD, "1.5");
    h.price(DE40, "20000");
    // The cap is 50% of 100m NAV: 1,700 contracts are 34m EUR but 51m USD.
    assert_eq!(rejection(&h.market_order(a, DE40, Side::Buy, "1700", "20000")), &Reject::HouseCapacity);
    assert_eq!(fills(&h.market_order(a, DE40, Side::Buy, "1600", "20000")).len(), 1);
    assert_eq!(h.engine.house_gross_exposure(), f("48000000"));
    // 48m USD uses over 80% of the cap, where 32m EUR would use 64%.
    let ev = h.price(DE40, "20000");
    assert!(ev.contains(&Event::StressChanged { market: DE40, level: 1 }), "{ev:?}");
}

#[test]
fn an_fx_move_alone_raises_the_stress_of_a_market_that_converts_at_it() {
    let (mut h, a) = (Harness::fx(), acct(1));
    h.deposit(a, "10000000");
    h.price(EURUSD, "1");
    h.price(DE40, "20000");
    // 32m EUR is 32m USD, 64% of the 50m cap; at 1.30 it is 41.6m, over the 80% trigger.
    h.market_order(a, DE40, Side::Buy, "1600", "20000");
    assert_eq!(h.engine.markets()[&DE40].stress.level, 0);
    let ev = h.price(EURUSD, "1.3");
    assert!(ev.contains(&Event::StressChanged { market: DE40, level: 1 }), "{ev:?}");
}

#[test]
fn financing_is_charged_in_usd() {
    let (mut h, a) = (Harness::fx(), acct(1));
    h.deposit(a, "100000");
    h.price(EURUSD, "1.08");
    h.price(DE40, "20000");
    h.market_order(a, DE40, Side::Buy, "2", "20000");
    // At the 22:00 London rollover: 2.5% a year over a zero benchmark on 40,000 EUR for one day of
    // 365 is 2.739726028 EUR rounded up, or 2.95890411024 USD, charged rounded up.
    h.advance_to(ny(2026, 10, 20, 17, 30));
    let ev = h.cmd(Command::Tick);
    assert!(ev.contains(&Event::Financing { account: a, market: DE40, amount: f("2.958904111") }), "{ev:?}");
}

#[test]
fn an_fx_move_closes_out_accounts_holding_markets_that_convert_at_it() {
    let (mut h, a) = (Harness::fx(), acct(1));
    h.admin(AdminCommand::UpsertMarket(de40(100)));
    h.deposit(a, "2300");
    h.price(EURUSD, "1.1");
    h.price(DE40, "20000");
    h.market_order(a, DE40, Side::Buy, "10", "20000");
    // -1,000 EUR at 1.10 leaves 1,200 USD of equity, above half the 2,189 USD margin.
    let ev = h.price(DE40, "19900");
    assert!(!ev.iter().any(|e| matches!(e, Event::CloseOut { .. })), "{ev:?}");
    // At 1.20 the same loss is 1,200 USD, leaving 1,100 against a 1,194 USD close-out level.
    let ev = h.price(EURUSD, "1.2");
    assert!(ev.contains(&Event::CloseOut { account: a, equity: f("1100"), margin: f("2388") }), "{ev:?}");
    let fill = only_fill(&ev);
    assert!(fill.liquidation && fill.market == DE40, "{fill:?}");
    assert_eq!((fill.realised_pnl, fill.fx_index), (f("-1200"), Some(f("1.2"))));
    assert_eq!(h.balance(a), f("1100"));
}

#[test]
fn close_out_takes_the_largest_loser_in_usd() {
    let (mut h, a) = (Harness::fx(), acct(1));
    h.deposit(a, "305");
    h.price(USDJPY, "150");
    h.price(BTC, "100000");
    h.market_order(a, USDJPY, Side::Buy, "10000", "150");
    h.market_order(a, BTC, Side::Buy, "0.01", "100000");
    h.price(USDJPY, "149");
    // USD-JPY is down 10,000 JPY, about 67 USD; BTC is down 100 USD, so BTC closes first.
    let ev = h.price(BTC, "90000");
    assert!(ev.iter().any(|e| matches!(e, Event::CloseOut { .. })), "{ev:?}");
    assert_eq!(only_fill(&ev).market, BTC);
    assert!(h.engine.accounts()[&a].positions.contains_key(&USDJPY));
}

#[test]
fn a_market_quoted_in_another_currency_lists_only_from_v7() {
    let mut h = Harness::new();
    h.admin(AdminCommand::UpsertMarket(market(EURUSD, "EUR-USD", AssetClass::Fx, Schedule::AlwaysOpen, 200)));
    h.admin(AdminCommand::SetClientAuth(ClientAuth::Unsigned));
    let versions =
        [ProtocolVersion::V3, ProtocolVersion::V4, ProtocolVersion::V5, ProtocolVersion::V6, ProtocolVersion::V7];
    for version in versions {
        let ev = h.cmd(Command::Admin(AdminCommand::UpsertMarket(de40(500))));
        assert!(matches!(&ev[..], [Event::AdminRejected { reason }] if reason.contains("needs protocol V7")), "{ev:?}");
        h.admin(AdminCommand::ActivateProtocol { version });
    }
    h.admin(AdminCommand::UpsertMarket(de40(500)));
    assert_eq!(h.engine.markets()[&DE40].spec.quote_currency, QuoteCurrency::Eur { fx_market: EURUSD });
}

#[test]
fn a_currency_converts_only_at_the_listed_market_for_its_pair() {
    let mut h = Harness::fx();
    let listing = |id, symbol, class, quote_currency| MarketSpec {
        quote_currency,
        ..market(id, symbol, class, Schedule::AlwaysOpen, 500)
    };
    let (gbp, jpy) = (|fx_market| QuoteCurrency::Gbp { fx_market }, |fx_market| QuoteCurrency::Jpy { fx_market });
    let refused = [
        // A market that is not an FX pair, one that is not listed, and another pair.
        listing(30, "UK100", AssetClass::Index, gbp(BTC)),
        listing(30, "UK100", AssetClass::Index, gbp(UNLISTED)),
        listing(30, "UK100", AssetClass::Index, gbp(EURUSD)),
        listing(31, "JP225", AssetClass::Index, jpy(EURUSD)),
        // Only USD-JPY converts at itself.
        listing(30, "UK100", AssetClass::Index, gbp(30)),
        listing(31, "JP225", AssetClass::Index, jpy(31)),
    ];
    for spec in refused {
        let ev = h.cmd(Command::Admin(AdminCommand::UpsertMarket(spec)));
        assert!(matches!(&ev[..], [Event::AdminRejected { reason }] if reason.contains("is not the listed")), "{ev:?}");
    }
    // Each pair is quoted in its second currency: EUR-USD and GBP-USD in USD, USD-JPY in yen at itself.
    let misquoted = [
        listing(EURUSD, "EUR-USD", AssetClass::Fx, QuoteCurrency::Eur { fx_market: EURUSD }),
        listing(EURUSD, "EUR-USD", AssetClass::Fx, jpy(USDJPY)),
        listing(32, "USD-JPY", AssetClass::Fx, QuoteCurrency::Usd),
    ];
    for spec in misquoted {
        let ev = h.cmd(Command::Admin(AdminCommand::UpsertMarket(spec)));
        assert!(matches!(&ev[..], [Event::AdminRejected { reason }] if reason.contains("must be quoted as")), "{ev:?}");
    }
    h.admin(AdminCommand::UpsertMarket(market(GBPUSD, "GBP-USD", AssetClass::Fx, Schedule::AlwaysOpen, 200)));
    h.admin(AdminCommand::UpsertMarket(listing(30, "UK100", AssetClass::Index, gbp(GBPUSD))));
    h.admin(AdminCommand::UpsertMarket(listing(31, "JP225", AssetClass::Index, jpy(USDJPY))));
    // Germany 40 converts at EUR-USD, so EUR-USD cannot be renamed to another pair.
    let renamed = market(EURUSD, "GBP-USD", AssetClass::Fx, Schedule::AlwaysOpen, 200);
    let ev = h.cmd(Command::Admin(AdminCommand::UpsertMarket(renamed)));
    assert!(matches!(&ev[..], [Event::AdminRejected { reason }] if reason.contains("converts here")), "{ev:?}");
    h.admin(AdminCommand::UpsertMarket(market(EURUSD, "EUR-USD", AssetClass::Fx, Schedule::AlwaysOpen, 300)));
}

#[test]
fn the_quote_currency_of_a_held_market_cannot_change() {
    let (mut h, a) = (Harness::fx(), acct(1));
    h.admin(AdminCommand::UpsertMarket(market(GBPUSD, "GBP-USD", AssetClass::Fx, Schedule::AlwaysOpen, 200)));
    h.deposit(a, "100000");
    h.price(EURUSD, "1.08");
    h.price(GBPUSD, "1.3");
    h.price(DE40, "20000");
    h.market_order(a, DE40, Side::Buy, "1", "20000");
    let pounds = MarketSpec { quote_currency: QuoteCurrency::Gbp { fx_market: GBPUSD }, ..de40(500) };
    let ev = h.cmd(Command::Admin(AdminCommand::UpsertMarket(pounds.clone())));
    assert!(matches!(&ev[..], [Event::AdminRejected { reason }] if reason.contains("open positions")), "{ev:?}");
    // Other changes keep the FX mark.
    h.admin(AdminCommand::UpsertMarket(de40(400)));
    assert_eq!(h.engine.markets()[&DE40].fx_mark, Some(f("1.08")));

    // Once flat, the currency may change, and the old FX mark goes: nothing deals until GBP-USD
    // marks the market.
    h.market_order(a, DE40, Side::Sell, "1", "20000");
    h.admin(AdminCommand::UpsertMarket(pounds));
    assert_eq!(h.engine.markets()[&DE40].fx_mark, None);
    let ev = h.market_order(a, DE40, Side::Buy, "1", "20000");
    assert_eq!(rejection(&ev), &Reject::NoFxPrice { fx_market: GBPUSD });
    h.price(GBPUSD, "1.3");
    let fill = only_fill(&h.market_order(a, DE40, Side::Buy, "1", "20000")).clone();
    assert_eq!(fill.fx_index, Some(f("1.3")));
}

#[test]
fn usd_markets_and_fills_keep_their_encoding() {
    let (mut h, a) = (Harness::fx(), acct(1));
    h.deposit(a, "100000");
    h.price(BTC, "100000");
    h.price(EURUSD, "1.08");
    h.price(DE40, "20000");
    let btc = serde_json::to_string(&h.market_order(a, BTC, Side::Buy, "0.1", "100000")).unwrap();
    let de40 = serde_json::to_string(&h.market_order(a, DE40, Side::Buy, "1", "20000")).unwrap();
    assert!(!btc.contains("fx_index") && de40.contains(r#""fx_index":"1.08""#), "{btc}\n{de40}");
    let state = |id| serde_json::to_string(&h.engine.markets()[&id]).unwrap();
    assert!(!state(BTC).contains("fx_mark") && !state(BTC).contains("quote_currency"), "{}", state(BTC));
    assert!(state(DE40).contains(r#""fx_mark":"1.08""#) && state(DE40).contains("quote_currency"), "{}", state(DE40));
}
