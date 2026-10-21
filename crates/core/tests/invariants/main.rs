//! Accounting invariants under generated sessions. Each case sequences random client, feed,
//! custody, and operator commands, with values inside and outside the engine's input bounds, under
//! `V2`, the rules every log starts under, and again under `V6`, under `V7` with markets quoted in
//! other currencies, and under `V10`. It checks after every command that
//! it did not panic, that cash is conserved, and that each market's aggregates match its accounts'
//! positions. Under each version the same sessions must reach every path that moves cash and the
//! refusal of out-of-range amounts, order numbers, and quotes, and under `V6` a close-out that no
//! price triggered, so the checks cannot pass vacuously.

mod charges;
#[path = "../common/mod.rs"]
mod common;
mod rounding;
mod session;
mod state_root;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use common::{HOUSE_CAPITAL, Harness, acct};
use engine_core::command::Command;
use engine_core::event::{Event, Reject};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::MarketId;
use engine_core::{Engine, Fixed};
use proptest::test_runner::{Config, RngSeed, TestCaseError, TestRunner};
use session::{Session, command, session};

/// Seed for every run, so CI and local runs check the same sessions.
const SEED: u64 = 0x5eed_0021;
/// Sessions per run: enough to find edge-of-range panics, few enough to run in about a second.
const CASES: u32 = 1_024;

#[test]
fn generated_sessions_hold_the_invariants_and_reach_every_checked_path() {
    // Coverage is recorded per protocol version, from the same sessions whose invariants are
    // checked.
    let reached: RefCell<BTreeMap<ProtocolVersion, BTreeSet<&str>>> = RefCell::default();
    let result = TestRunner::new(config()).run(&session(), |session| {
        for genesis in [Harness::new(), Harness::v6(), Harness::fx(), Harness::v10()] {
            let mut h = funded(genesis, &session);
            let version = h.engine.protocol_version();
            let mut inflow = h.deposits + Fixed::from_int(HOUSE_CAPITAL);
            check(&h.engine, inflow).map_err(TestCaseError::fail)?;
            for step in &session.steps {
                h.advance(step.advance_ms);
                let cmd = command(&step.action, &h.engine, h.ts);
                // `Harness::cmd` panics if the engine refuses the sequence; a panic fails the case.
                let events = h.cmd(cmd.clone());
                let mut reached = reached.borrow_mut();
                let paths = reached.entry(version).or_default();
                paths.extend(events.iter().map(path));
                if !matches!(cmd, Command::Prices { .. }) && events.iter().any(|e| matches!(e, Event::CloseOut { .. }))
                {
                    paths.insert("close-out without a price");
                }
                inflow += cash_moved(&events);
                check(&h.engine, inflow)
                    .map_err(|e| TestCaseError::fail(format!("{version:?}, after {cmd:?}: {e}")))?;
            }
        }
        Ok(())
    });
    if let Err(failure) = result {
        panic!("{failure}");
    }
    let expected = [
        "deposit",
        "deposit rejected",
        "deposit out of range",
        "withdrawal",
        "withdrawal rejected",
        "withdrawal out of range",
        "house capital",
        "house capital out of range",
        "fill",
        "fill with profit fee",
        "liquidation",
        "bad debt",
        "financing",
        "order out of range",
        "malformed quote",
        "quote stamped ahead",
        "admin applied",
        "admin rejected",
    ];
    let reached = reached.into_inner();
    let missed: Vec<(ProtocolVersion, &str)> =
        [ProtocolVersion::V2, ProtocolVersion::V6, ProtocolVersion::V7, ProtocolVersion::V10]
            .into_iter()
            .flat_map(|version| expected.iter().map(move |path| (version, *path)))
            .chain([(ProtocolVersion::V6, "close-out without a price")])
            // Only `V7` lists markets quoted in another currency.
            .chain([(ProtocolVersion::V7, "fill converted to USD"), (ProtocolVersion::V7, "no FX price")])
            .filter(|(version, path)| !reached.get(version).is_some_and(|paths| paths.contains(path)))
            .collect();
    assert!(missed.is_empty(), "generated sessions never reach {missed:?}");
}

// ── Helpers ──

/// `CASES` sessions from `SEED`, without writing regression files.
fn config() -> Config {
    Config { cases: CASES, rng_seed: RngSeed::Fixed(SEED), failure_persistence: None, ..Config::default() }
}

/// `h` after its genesis, with the session's deposits for the three traders.
fn funded(mut h: Harness, session: &Session) -> Harness {
    for (n, amount) in (1..).zip(session.funding) {
        h.deposit(acct(n), &amount.to_string());
    }
    h
}

/// Fails unless client balances plus house cash equal the cash that has entered and left, and every
/// market's aggregate qty and cost equal the sum over its accounts' positions.
/// Error cases:
/// - cash is not conserved
/// - a market's aggregates differ from its positions, or a position is in an unlisted market
fn check(engine: &Engine, inflow: Fixed) -> Result<(), String> {
    let balances: Fixed = engine.accounts().values().map(|acct| acct.balance).sum();
    if balances + engine.house_cash() != inflow {
        return Err(format!("cash not conserved: balances {balances} + house {} != {inflow}", engine.house_cash()));
    }
    let mut held: BTreeMap<MarketId, (Fixed, Fixed)> = BTreeMap::new();
    for (market, pos) in engine.accounts().values().flat_map(|acct| &acct.positions) {
        let (qty, cost) = held.entry(*market).or_default();
        *qty += pos.qty;
        *cost += pos.cost();
    }
    for (id, market) in engine.markets() {
        let (qty, cost) = held.remove(id).unwrap_or_default();
        if (market.client_qty, market.client_cost) != (qty, cost) {
            return Err(format!(
                "market {id} aggregates {} / {} != positions {qty} / {cost}",
                market.client_qty, market.client_cost
            ));
        }
    }
    match held.keys().next() {
        Some(id) => Err(format!("positions in unlisted market {id}")),
        None => Ok(()),
    }
}

/// Cash the events moved across the engine's boundary: deposits and house capital in, approved
/// withdrawals out.
fn cash_moved(events: &[Event]) -> Fixed {
    events
        .iter()
        .map(|event| match event {
            Event::Deposited { amount, .. } | Event::HouseCapitalMoved { amount, .. } => *amount,
            Event::WithdrawalApproved { amount, .. } => -*amount,
            _ => Fixed::ZERO,
        })
        .sum()
}

/// The path an event shows a session reached, for the coverage check.
fn path(event: &Event) -> &'static str {
    match event {
        Event::Deposited { .. } => "deposit",
        Event::DepositRejected { reason: Reject::InvalidAmount, .. } => "deposit out of range",
        Event::DepositRejected { .. } => "deposit rejected",
        Event::WithdrawalApproved { .. } => "withdrawal",
        Event::WithdrawalRejected { reason: Reject::InvalidAmount, .. } => "withdrawal out of range",
        Event::WithdrawalRejected { .. } => "withdrawal rejected",
        Event::HouseCapitalMoved { .. } => "house capital",
        Event::Fill(fill) if fill.liquidation => "liquidation",
        Event::Fill(fill) if fill.profit_fee.is_positive() => "fill with profit fee",
        Event::Fill(fill) if fill.fx_index.is_some() => "fill converted to USD",
        Event::Fill(_) => "fill",
        Event::BadDebt { .. } => "bad debt",
        Event::Financing { .. } => "financing",
        Event::OrderRejected { reason: Reject::InvalidOrder { reason }, .. } if reason.ends_with("out of range") => {
            "order out of range"
        }
        Event::OrderRejected { reason: Reject::NoFxPrice { .. }, .. } => "no FX price",
        Event::PricesRejected { reason, .. } if reason.starts_with("malformed") => "malformed quote",
        Event::PricesRejected { reason, .. } if reason.ends_with("ahead of the sequenced time") => {
            "quote stamped ahead"
        }
        Event::AdminApplied => "admin applied",
        Event::AdminRejected { reason } if reason.ends_with("amount out of range") => "house capital out of range",
        // House capital refusals come back as admin rejections; keep them apart from operator ones.
        Event::AdminRejected { reason } if reason.starts_with("house capital") => "house capital rejected",
        Event::AdminRejected { .. } => "admin rejected",
        _ => "other",
    }
}
