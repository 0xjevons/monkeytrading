//! Accounting invariants under generated sessions. Each case sequences random client, feed,
//! custody, and operator commands, with values inside and outside the engine's input bounds, and
//! checks after every command that it did not panic, that cash is conserved, and that each market's
//! aggregates match its accounts' positions. The same sessions must reach every path that moves cash
//! and every kind of out-of-bounds input, so the checks cannot pass vacuously.

#[path = "../common/mod.rs"]
mod common;
mod session;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use common::{HOUSE_CAPITAL, Harness, acct};
use engine_core::event::{Event, Reject};
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
    // Coverage is recorded from the same sessions whose invariants are checked.
    let reached = RefCell::new(BTreeSet::new());
    let result = TestRunner::new(config()).run(&session(), |session| {
        let mut h = funded(&session);
        let mut inflow = h.deposits + Fixed::from_int(HOUSE_CAPITAL);
        check(&h.engine, inflow).map_err(TestCaseError::fail)?;
        for step in session.steps {
            h.advance(step.advance_ms);
            let cmd = command(&step.action, &h.engine, h.ts);
            // `Harness::cmd` panics if the engine refuses the sequence; a panic fails the case.
            let events = h.cmd(cmd.clone());
            reached.borrow_mut().extend(events.iter().map(path));
            inflow += cash_moved(&events);
            check(&h.engine, inflow).map_err(|e| TestCaseError::fail(format!("after {cmd:?}: {e}")))?;
        }
        Ok(())
    });
    if let Err(failure) = result {
        panic!("{failure}");
    }
    let expected = [
        "deposit",
        "deposit rejected",
        "withdrawal",
        "withdrawal rejected",
        "house capital",
        "fill",
        "fill with profit fee",
        "liquidation",
        "bad debt",
        "financing",
        "order out of bounds",
        "malformed quote",
        "admin applied",
        "admin rejected",
    ];
    let reached = reached.into_inner();
    let missed: Vec<&str> = expected.into_iter().filter(|p| !reached.contains(p)).collect();
    assert!(missed.is_empty(), "generated sessions never reach {missed:?}");
}

// ── Helpers ──

/// `CASES` sessions from `SEED`, without writing regression files.
fn config() -> Config {
    Config { cases: CASES, rng_seed: RngSeed::Fixed(SEED), failure_persistence: None, ..Config::default() }
}

/// Genesis, then the session's deposits for the three traders.
fn funded(session: &Session) -> Harness {
    let mut h = Harness::new();
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
        Event::DepositRejected { .. } => "deposit rejected",
        Event::WithdrawalApproved { .. } => "withdrawal",
        Event::WithdrawalRejected { .. } => "withdrawal rejected",
        Event::HouseCapitalMoved { .. } => "house capital",
        Event::Fill(fill) if fill.liquidation => "liquidation",
        Event::Fill(fill) if fill.profit_fee.is_positive() => "fill with profit fee",
        Event::Fill(_) => "fill",
        Event::BadDebt { .. } => "bad debt",
        Event::Financing { .. } => "financing",
        Event::OrderRejected { reason: Reject::InvalidOrder { reason }, .. } if reason.ends_with("out of range") => {
            "order out of bounds"
        }
        Event::PricesRejected { reason, .. } if reason.starts_with("malformed") => "malformed quote",
        Event::AdminApplied => "admin applied",
        Event::AdminRejected { .. } => "admin rejected",
        _ => "other",
    }
}
