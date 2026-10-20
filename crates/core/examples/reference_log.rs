//! Writes the reference log for a new protocol version: a scripted session through every command
//! and the engine's main paths, with the state root after each command. Run it once per protocol
//! version and commit the output; it refuses to overwrite a log that already exists.
//!
//! `cargo run -p engine-core --example reference_log -- crates/core/tests/reference/v<N>.jsonl`

#[path = "../tests/common/mod.rs"]
mod common;

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{BufWriter, ErrorKind, Write};
use std::path::PathBuf;

use anyhow::anyhow;
use chrono::NaiveDate;
use common::reference::{ReferenceEntry, root_hex};
use common::{AAPL, BTC, Client, ETH, Harness, UNLISTED, acct, f, market, ny, params, us_equities};
use engine_core::Engine;
use engine_core::auth::MAX_EXPIRY_AHEAD_MS;
use engine_core::command::{AdminCommand, Command, OrderKind, OrderRequest, Sequenced};
use engine_core::event::Event;
use engine_core::market::Tier;
use engine_core::params::{ProtocolParams, ToxicityParams};
use engine_core::price::{MAX_QUOTE_LEAD_MS, SourceQuote};
use engine_core::protocol::ProtocolVersion;
use engine_core::session::{Period, Schedule};
use engine_core::types::{AccountId, AssetClass, MarketId, OrderId, Side};

/// A market the session lists with two margin bands.
const SOL: MarketId = 10;

/// Writes the session to the path given as the only argument, then prints how often each event
/// kind occurred, so the author can check the session still reaches every path.
/// Error cases:
/// - no output path, or a file already at it
/// - the file cannot be written
fn main() -> anyhow::Result<()> {
    let path: PathBuf =
        std::env::args_os().nth(1).ok_or_else(|| anyhow!("usage: reference_log <output.jsonl>"))?.into();
    let mut engine = Engine::new();
    let mut tally: BTreeMap<String, usize> = BTreeMap::new();
    let mut entries = Vec::new();
    for sequenced in session() {
        for event in engine.apply(&sequenced)? {
            *tally.entry(event_kind(&event)).or_default() += 1;
        }
        entries.push(ReferenceEntry { sequenced, root: root_hex(engine.state_root()) });
    }

    let file = OpenOptions::new().write(true).create_new(true).open(&path).map_err(|e| match e.kind() {
        ErrorKind::AlreadyExists => {
            anyhow!("{} exists; a committed reference log is never regenerated", path.display())
        }
        _ => anyhow!(e).context(format!("creating {}", path.display())),
    })?;
    let mut out = BufWriter::new(file);
    for entry in &entries {
        serde_json::to_writer(&mut out, entry)?;
        out.write_all(b"\n")?;
    }
    out.flush()?;
    println!("wrote {} commands to {}", entries.len(), path.display());
    tally.iter().for_each(|(kind, count)| println!("{count:>5} {kind}"));
    Ok(())
}

/// The scripted session under the latest protocol version: client auth and activation, genesis,
/// then funding, prices, every order kind, signed requests refused for each auth reason, triggers
/// (one held back by its spread), cancels, reduce-only (one clipped to the position), stress,
/// toxicity, a position flip, house capital, withdrawals, a session close and gapped open, financing
/// rollovers over longs and a short, a two-position close-out with bad debt, two-band margin, an
/// exchange holiday, an early close and a special session, admin changes, and venue quotes stamped
/// ahead of the sequenced time, with a rejection of each kind along the way. Every client request is
/// signed by its account's key unless the comment says otherwise.
fn session() -> Vec<Sequenced> {
    let mut h = Harness::signed();
    let (a, b, c, d, t, e) = (h.signer(1), h.signer(2), h.signer(3), h.signer(4), h.signer(5), h.signer(6));
    let stranger = h.signer(9);
    h.admin(AdminCommand::SetBenchmarkRate { benchmark: "SOFR".into(), annual_bps: 430 });

    // Funding, and deposits refused as a duplicate reference or out of range.
    h.deposit(a, "50000.123456789");
    h.deposit(b, "20000");
    h.deposit(c, "1000");
    h.deposit(d, "20000000");
    h.deposit(t, "10000");
    h.deposit(e, "20000");
    let deposit =
        |amount: &str, chain_ref: &str| Command::Deposit { account: a, amount: f(amount), chain_ref: chain_ref.into() };
    h.cmd(deposit("250.5", "0xfeed:1"));
    h.cmd(deposit("250.5", "0xfeed:1"));
    h.cmd(deposit("0", "0xfeed:2"));
    h.cmd(deposit("1000000000.000000001", "0xfeed:3"));

    // Prices: an odd and an even venue count, an unlisted market, and a malformed quote.
    h.quotes(BTC, &["100000.123456789", "100000.2", "99999.9"]);
    h.quotes(ETH, &["3000.5", "3000.7"]);
    h.price(AAPL, "230.25");
    h.price(UNLISTED, "1");
    let malformed = SourceQuote { source: "venue0".into(), mid: f("-1"), ts: h.ts };
    h.cmd(Command::Prices { market: BTC, quotes: vec![malformed] });

    // Market orders: fills, a slippage decline, a client-tightened bound, and refusals.
    h.advance(100);
    h.market_order(a, BTC, Side::Buy, "0.123456789", "100000.123456789");
    h.market_order(a, BTC, Side::Buy, "0.1", "99000");
    h.order(b, ETH, Side::Sell, "3.3", OrderKind::Market { quoted: f("3000.6"), max_slippage_bps: Some(5) });
    h.market_order(c, BTC, Side::Buy, "1", "100000.123456789");
    h.market_order(stranger, BTC, Side::Buy, "0.01", "100000.123456789");
    h.market_order(a, BTC, Side::Buy, "0", "100000.123456789");
    h.market_order(a, UNLISTED, Side::Buy, "1", "1");

    // Signed requests refused: unsigned, signed by another key, a reused nonce, an expired request,
    // and an expiry too far ahead. The last two use their nonces.
    h.market_order(acct(8), BTC, Side::Buy, "0.01", "100000.123456789");
    let key_a = |expires_at| Client { expires_at, ..Client::new(1) };
    let forger = Client { expires_at: h.ts + 60_000, ..Client::new(7) };
    h.cmd(forger.order(h.engine.next_request_nonce(a), small_buy(a)));
    h.cmd(key_a(h.ts + 60_000).order(0, small_buy(a)));
    h.cmd(key_a(h.ts - 1).order(h.engine.next_request_nonce(a), small_buy(a)));
    h.cmd(key_a(h.ts + MAX_EXPIRY_AHEAD_MS + 1).order(h.engine.next_request_nonce(a), small_buy(a)));

    // Resting orders: a limit and a stop that trigger, one cancelled, and an unknown cancel.
    h.order(a, BTC, Side::Sell, "0.05", OrderKind::Limit { price: f("101000") });
    h.order(b, ETH, Side::Buy, "1", OrderKind::Stop { price: f("3050") });
    let cancelled = accepted(&h.order(a, BTC, Side::Buy, "0.01", OrderKind::Limit { price: f("90000") }));
    h.cmd(Command::CancelOrder { account: a, order_id: cancelled, auth: None });
    h.cmd(Command::CancelOrder { account: a, order_id: 999, auth: None });
    h.advance(500);
    h.quotes(BTC, &["101200", "101250", "101210"]);
    h.price(ETH, "3060");

    // Reduce-only: new risk refused in a reduce-only market, a reducing order allowed.
    h.admin(AdminCommand::SetReduceOnly { market: ETH, reduce_only: true });
    h.market_order(b, ETH, Side::Sell, "1", "3060");
    h.cmd(Command::PlaceOrder(reducing(b, ETH, Side::Buy, "0.5", "3060")));
    h.admin(AdminCommand::SetReduceOnly { market: ETH, reduce_only: false });

    // Stress: an operator floor, a fill paying its spread, then a divergence trigger and cooldown.
    h.admin(AdminCommand::SetStressFloor { market: Some(BTC), level: 1 });
    h.market_order(a, BTC, Side::Buy, "0.01", "101210");
    // A buy limit at the mid triggers, but the 5bps spread prices it above the limit: it keeps
    // resting until a later price brings the fill inside it.
    h.order(a, BTC, Side::Buy, "0.01", OrderKind::Limit { price: f("101210") });
    h.admin(AdminCommand::SetStressFloor { market: Some(BTC), level: 0 });
    h.cmd(Command::Admin(AdminCommand::SetStressFloor { market: None, level: 5 }));
    h.quotes(BTC, &["101000", "101150"]);
    h.advance(301_000);
    h.quotes(BTC, &["101000", "101010"]);

    // Toxicity: a fill marked out 49bps in the trader's favour lifts the score into the 3bps tier.
    h.price(ETH, "3060");
    h.market_order(t, ETH, Side::Buy, "1", "3060");
    h.advance(1_000);
    h.price(ETH, "3075");
    h.market_order(t, ETH, Side::Buy, "1", "3075");
    // A buy through a short: the 1.8 short closes at a loss and a 1.2 long opens at the fill price.
    h.market_order(b, ETH, Side::Buy, "3", "3075");
    // A short held over the rollover, which receives the benchmark less the markup.
    h.market_order(e, ETH, Side::Sell, "2", "3075");

    // Size spread on a large fill, then the house cap refusing a larger one.
    h.price(BTC, "101000");
    h.market_order(d, BTC, Side::Buy, "30", "101000");
    h.market_order(d, BTC, Side::Buy, "600", "101000");

    // House capital in and out, a duplicate reference, and a move out that would leave NAV at zero or
    // below.
    let capital =
        |amount: &str, chain_ref: &str| Command::HouseCapital { amount: f(amount), chain_ref: chain_ref.into() };
    h.cmd(capital("5000000", "0xcap:1"));
    h.cmd(capital("-1000000", "0xcap:2"));
    h.cmd(capital("1", "0xcap:1"));
    h.cmd(capital("-1000000000", "0xcap:3"));

    // Withdrawals: approved, beyond free cash, and out of range.
    h.cmd(Command::Withdraw { account: a, amount: f("1000"), auth: None });
    h.cmd(Command::Withdraw { account: c, amount: f("1000000"), auth: None });
    h.cmd(Command::Withdraw { account: a, amount: f("0"), auth: None });

    // Venue hours: a stop rests over the close, nothing trades while shut, and the stop fills at
    // the gapped open. The financing rollover at 22:00 London falls in between.
    h.price(AAPL, "230.25");
    h.market_order(a, AAPL, Side::Buy, "10", "230.25");
    h.order(a, AAPL, Side::Sell, "10", OrderKind::Stop { price: f("225") });
    h.advance_to(ny(2026, 10, 20, 16, 30));
    h.cmd(Command::Tick);
    h.market_order(a, AAPL, Side::Buy, "1", "230");
    h.advance_to(ny(2026, 10, 20, 17, 30));
    h.cmd(Command::Tick);
    // BTC has had no quote for hours, so it has no price.
    h.market_order(a, BTC, Side::Buy, "0.01", "101000");
    h.advance_to(ny(2026, 10, 21, 9, 30));
    h.price(AAPL, "220");

    // Leveraged longs gapped through the balance: a close-out that takes BTC (the larger loser) then
    // ETH, and bad debt.
    h.price(BTC, "101000");
    h.price(ETH, "3075");
    h.market_order(c, ETH, Side::Buy, "0.2", "3075");
    h.market_order(c, BTC, Side::Buy, "0.09", "101000");
    h.advance(100);
    h.price(BTC, "80000");
    // A limit the account cannot margin triggers at once and is cancelled as unfillable.
    h.order(c, BTC, Side::Buy, "1", OrderKind::Limit { price: f("81000") });
    // Venues 62bps apart: no index, so nothing fills.
    h.quotes(BTC, &["80000", "80500"]);
    h.market_order(a, BTC, Side::Sell, "0.01", "80250");

    // Admin: new parameters and listings, and refusals of each kind.
    h.admin(AdminCommand::SetParams(ProtocolParams { profit_fee_bps: 1_500, ..params() }));
    h.cmd(Command::Admin(AdminCommand::SetParams(ProtocolParams { closeout_level_bps: 0, ..params() })));
    let toxicity = ToxicityParams { horizon_ms: 3_600_001, ..params().toxicity };
    h.cmd(Command::Admin(AdminCommand::SetParams(ProtocolParams { toxicity, ..params() })));
    let mut eth = market(ETH, "ETH-USD", AssetClass::Crypto, Schedule::AlwaysOpen, 1_000);
    eth.min_order_notional = f("100");
    h.admin(AdminCommand::UpsertMarket(eth));
    h.price(ETH, "3075");
    // A reduce-only sell of 5 against a long of 2 is clipped to 2.
    h.cmd(Command::PlaceOrder(reducing(t, ETH, Side::Sell, "5", "3075")));
    h.market_order(b, ETH, Side::Buy, "0.01", "3075");
    // Margin of 10% up to 10,000 notional and 20% above: a 15,000 position needs 2,000.
    let mut sol = market(SOL, "SOL-USD", AssetClass::Crypto, Schedule::AlwaysOpen, 1_000);
    sol.margin_tiers = vec![Tier { up_to: Some(f("10000")), bps: 1_000 }, Tier { up_to: None, bps: 2_000 }];
    h.admin(AdminCommand::UpsertMarket(sol));
    h.price(SOL, "150");
    h.market_order(a, SOL, Side::Buy, "100", "150");
    h.cmd(Command::Admin(AdminCommand::SetBenchmarkRate { benchmark: "SOFR".into(), annual_bps: 10_001 }));
    h.cmd(Command::Admin(AdminCommand::SetReduceOnly { market: UNLISTED, reduce_only: true }));
    // An exchange holiday on Thursday keeps AAPL shut through its session hours.
    let Schedule::Weekly { tz, windows, .. } = us_equities() else { unreachable!("US equities keep weekly hours") };
    let holiday = NaiveDate::from_ymd_opt(2026, 10, 22).expect("a real date");
    let schedule = Schedule::Weekly {
        tz,
        windows: windows.clone(),
        closed_dates: vec![holiday],
        closures: vec![],
        special_sessions: vec![],
    };
    h.admin(AdminCommand::UpsertMarket(market(AAPL, "AAPL", AssetClass::Equity, schedule, 2_000)));
    h.advance_to(ny(2026, 10, 22, 10, 0));
    h.cmd(Command::Tick);

    // Venue clocks: a quote stamped at the lead limit is kept, one a millisecond further ahead is
    // refused and leaves the venue's stale quote in place, and the first venue's next quote, stamped
    // earlier than its kept one, replaces it and prices the market.
    let ts = h.ts;
    let ahead = |source: &str, lead_ms| SourceQuote { source: source.into(), mid: f("80000"), ts: ts + lead_ms };
    h.cmd(Command::Prices {
        market: BTC,
        quotes: vec![ahead("venue0", MAX_QUOTE_LEAD_MS), ahead("venue1", MAX_QUOTE_LEAD_MS + 1)],
    });
    h.advance(100);
    h.quotes(BTC, &["80000", "80010"]);

    // V4 activates at this seq, then a calendar update: Friday closes at 13:00 and Saturday trades a
    // special session. An order fills before the early close, is refused after it, and fills again in
    // the special session.
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V4 });
    let at = |local: &str| local.parse().expect("a local date and time");
    let schedule = Schedule::Weekly {
        tz,
        windows,
        closed_dates: vec![holiday],
        closures: vec![Period { from: at("2026-10-23T13:00:00"), to: at("2026-10-23T16:00:00") }],
        special_sessions: vec![Period { from: at("2026-10-24T10:00:00"), to: at("2026-10-24T12:00:00") }],
    };
    h.admin(AdminCommand::UpsertMarket(market(AAPL, "AAPL", AssetClass::Equity, schedule, 2_000)));
    h.advance_to(ny(2026, 10, 23, 12, 59));
    h.price(AAPL, "201");
    h.market_order(a, AAPL, Side::Buy, "1", "201");
    h.advance_to(ny(2026, 10, 23, 13, 0));
    h.cmd(Command::Tick);
    h.market_order(a, AAPL, Side::Buy, "1", "201");
    h.advance_to(ny(2026, 10, 24, 10, 0));
    h.price(AAPL, "199");
    h.market_order(a, AAPL, Side::Sell, "1", "199");
    h.advance_to(ny(2026, 10, 24, 12, 0));
    h.cmd(Command::Tick);
    h.log
}

/// A reduce-only market order quoted at `quoted`.
fn reducing(account: AccountId, market: MarketId, side: Side, qty: &str, quoted: &str) -> OrderRequest {
    OrderRequest {
        account,
        client_id: 8,
        market,
        side,
        qty: f(qty),
        kind: OrderKind::Market { quoted: f(quoted), max_slippage_bps: None },
        reduce_only: true,
        auth: None,
    }
}

/// A 0.01 BTC market buy for `account`, quoted at the session's first BTC mid, unsigned.
fn small_buy(account: AccountId) -> OrderRequest {
    OrderRequest {
        account,
        client_id: 9,
        market: BTC,
        side: Side::Buy,
        qty: f("0.01"),
        kind: OrderKind::Market { quoted: f("100000.123456789"), max_slippage_bps: None },
        reduce_only: false,
        auth: None,
    }
}

/// The order id an `OrderAccepted` among `events` carries; panics if there is none.
fn accepted(events: &[Event]) -> OrderId {
    events
        .iter()
        .find_map(|e| match e {
            Event::OrderAccepted { order_id, .. } => Some(*order_id),
            _ => None,
        })
        .unwrap_or_else(|| panic!("expected an accepted order, got {events:?}"))
}

/// The event's `type` tag, plus the rejection kind where there is one.
fn event_kind(event: &Event) -> String {
    let json = serde_json::to_value(event).expect("events serialise");
    let kind = json["type"].as_str().unwrap_or_default().to_owned();
    match json["reason"]["kind"].as_str() {
        Some(reason) => format!("{kind}:{reason}"),
        None => kind,
    }
}
