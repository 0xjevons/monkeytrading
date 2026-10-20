//! Writes the reference log for a new protocol version: a scripted session through every command
//! and the engine's main paths, with the state root after each command. Run it once per protocol
//! version and commit the output; it refuses to overwrite a log that already exists.
//!
//! `cargo run -p engine-core --example reference_log -- crates/core/tests/reference/v<N>.jsonl`

#[path = "../tests/common/mod.rs"]
mod common;

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

use anyhow::{Context, anyhow};
use common::reference::{ReferenceEntry, root_hex};
use common::{AAPL, BTC, ETH, Harness, acct, f, market, ny, params};
use engine_core::Engine;
use engine_core::command::{AdminCommand, Command, OrderKind, OrderRequest, Sequenced};
use engine_core::event::Event;
use engine_core::market::Tier;
use engine_core::params::ProtocolParams;
use engine_core::price::SourceQuote;
use engine_core::session::Schedule;
use engine_core::types::{AccountId, AssetClass, MarketId, OrderId, Side};

/// A market id nothing lists.
const UNLISTED: MarketId = 99;
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

    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| format!("{}: a committed reference log is never regenerated", path.display()))?;
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

/// The scripted session: genesis, then funding, prices, every order kind, triggers, cancels,
/// reduce-only, stress, toxicity, house capital, withdrawals, a session close and gapped open, a
/// financing rollover, a close-out with bad debt, and admin changes, with a rejection of each kind
/// along the way.
fn session() -> Vec<Sequenced> {
    let (a, b, c, d, t, stranger) = (acct(1), acct(2), acct(3), acct(4), acct(5), acct(9));
    let mut h = Harness::new();
    h.admin(AdminCommand::SetBenchmarkRate { benchmark: "SOFR".into(), annual_bps: 430 });

    // Funding, and deposits refused as a duplicate reference or out of range.
    h.deposit(a, "50000.123456789");
    h.deposit(b, "20000");
    h.deposit(c, "1000");
    h.deposit(d, "20000000");
    h.deposit(t, "10000");
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

    // Resting orders: a limit and a stop that trigger, one cancelled, and an unknown cancel.
    h.order(a, BTC, Side::Sell, "0.05", OrderKind::Limit { price: f("101000") });
    h.order(b, ETH, Side::Buy, "1", OrderKind::Stop { price: f("3050") });
    let cancelled = accepted(&h.order(a, BTC, Side::Buy, "0.01", OrderKind::Limit { price: f("90000") }));
    h.cmd(Command::CancelOrder { account: a, order_id: cancelled });
    h.cmd(Command::CancelOrder { account: a, order_id: 999 });
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
    h.cmd(Command::Withdraw { account: a, amount: f("1000") });
    h.cmd(Command::Withdraw { account: c, amount: f("1000000") });
    h.cmd(Command::Withdraw { account: a, amount: f("0") });

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

    // A leveraged long gapped through its balance: close-out, liquidation fill, and bad debt.
    h.price(BTC, "101000");
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
    let mut eth = market(ETH, "ETH-USD", AssetClass::Crypto, Schedule::AlwaysOpen, 1_000);
    eth.min_order_notional = f("100");
    h.admin(AdminCommand::UpsertMarket(eth));
    h.price(ETH, "3075");
    h.market_order(b, ETH, Side::Buy, "0.01", "3075");
    // Margin of 10% up to 10,000 notional and 20% above: a 15,000 position needs 2,000.
    let mut sol = market(SOL, "SOL-USD", AssetClass::Crypto, Schedule::AlwaysOpen, 1_000);
    sol.margin_tiers = vec![Tier { up_to: Some(f("10000")), bps: 1_000 }, Tier { up_to: None, bps: 2_000 }];
    h.admin(AdminCommand::UpsertMarket(sol));
    h.price(SOL, "150");
    h.market_order(a, SOL, Side::Buy, "100", "150");
    h.cmd(Command::Admin(AdminCommand::SetBenchmarkRate { benchmark: "SOFR".into(), annual_bps: 10_001 }));
    h.cmd(Command::Admin(AdminCommand::SetReduceOnly { market: UNLISTED, reduce_only: true }));
    h.advance(60_000);
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
