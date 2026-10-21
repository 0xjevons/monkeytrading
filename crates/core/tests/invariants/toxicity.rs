//! The toxicity score stays within a millionth of a bp of the exact notional-weighted average of its
//! markouts, from its neutral prior and over at most its memory, each markout taken at the index
//! valid when it is scored: generated fills by one account, of any size from one unit of precision
//! to many times the prior, each followed at its horizon by a move of any size, or by a window
//! without a valid index that the markout may outlast, under `V11`.

use std::cell::RefCell;
use std::collections::BTreeSet;

use engine_core::Fixed;
use engine_core::command::{AdminCommand, Command};
use engine_core::params::{NotionalWeighting, ProtocolParams, ToxicityParams};
use engine_core::types::Side;
use proptest::arbitrary::any;
use proptest::collection::vec;
use proptest::prop_oneof;
use proptest::sample::select;
use proptest::strategy::{Just, Strategy};
use proptest::test_runner::{Config, RngSeed, TestCaseError, TestRunner};

use crate::common::{BTC, Harness, acct, fills, params};

const SEED: u64 = 0x5eed_0006;
const CASES: u32 = 256;
/// The reference score's unit, a femto-bp: a millionth of the engine's nine decimals.
const PER_BP: i128 = 10i128.pow(15);
/// A millionth of a bp, in femto-bps.
const TOLERANCE: i128 = PER_BP / 1_000_000;
const HORIZON_MS: i64 = 1_000;

/// What the market shows when a fill's markout falls due, `delay_ms` past its horizon, before the
/// fresh index that follows at the same time.
#[derive(Clone, Copy, Debug)]
enum Window {
    /// Nothing: the fresh index is the first command.
    Fresh,
    /// A tick, with the fill's quotes stale by then.
    Tick,
    /// A fresh quote from one of the two venues the market needs, 1% from the index that follows.
    OneSource,
    /// Fresh quotes from venues 99 bps apart.
    Divergent,
}

#[derive(Clone, Copy, Debug)]
struct Step {
    side: Side,
    qty: Fixed,
    /// The index move from the fill to its scoring, in hundredths of a bp.
    move_cbps: i128,
    window: Window,
    delay_ms: i64,
}

#[test]
fn the_score_tracks_notional_weighted_markouts_at_the_index_when_scored() {
    let config =
        Config { cases: CASES, rng_seed: RngSeed::Fixed(SEED), failure_persistence: None, ..Config::default() };
    // Priors and memories, from a memory no larger than the prior to one a thousand times as large.
    let weightings = [(1, 1), (1, 1_000), (100, 100_000), (10_000, 10_000), (10_000, 10_000_000)];
    let weighting = select(weightings.to_vec()).prop_map(|(prior, memory)| NotionalWeighting {
        prior: Fixed::from_int(prior),
        memory: Fixed::from_int(memory),
    });
    // What the generated steps reached, so the check cannot pass vacuously.
    let reached: RefCell<BTreeSet<String>> = RefCell::default();
    let cases = (weighting, vec(step(), 1..=40));
    let result = TestRunner::new(config).run(&cases, |(weighting, steps)| {
        let mut h = Harness::v11();
        let toxicity = ToxicityParams { notional: Some(weighting), ..params().toxicity };
        h.operator_admin(AdminCommand::SetParams(ProtocolParams { toxicity, ..params() }));
        let a = acct(1);
        h.deposit(a, "10000000");
        let mut index = Fixed::from_int(100_000);
        // The reference score, in femto-bps, and the fill notional behind it, from the prior.
        let (mut expected, mut history) = (0i128, weighting.prior.raw());
        for step in steps {
            h.price(BTC, &index.to_string());
            let events = h.market_order(a, BTC, step.side, &step.qty.to_string(), &index.to_string());
            let Some(fill) = fills(&events).first().map(|fill| (*fill).clone()) else { continue };
            let moved = Fixed::from_raw(index.raw() + index.raw() * step.move_cbps / 1_000_000);
            h.advance(HORIZON_MS + step.delay_ms);
            let quote = moved.to_string();
            match step.window {
                Window::Fresh => {}
                Window::Tick => {
                    h.cmd(Command::Tick);
                }
                Window::OneSource => {
                    h.quotes(BTC, &[&(moved + moved.mul_bps(100)).to_string()]);
                }
                Window::Divergent => {
                    h.quotes(BTC, &[&quote, &(moved + moved.mul_bps(100)).to_string()]);
                }
            }
            h.price(BTC, &quote);
            index = moved;
            if step.delay_ms <= HORIZON_MS {
                let markout = markout(fill.price, moved, step.side);
                // The notional-weighted average of the history and the fill.
                let notional = fill.qty.mul(fill.price).raw();
                expected += (markout - expected) * notional / (history + notional);
                let mut reached = reached.borrow_mut();
                reached.insert(
                    if history + notional > weighting.memory.raw() { "at memory" } else { "below memory" }.into(),
                );
                history = (history + notional).min(weighting.memory.raw());
                reached.insert(format!("scored after {:?}", step.window));
                if markout.abs() == 1_000 * PER_BP {
                    reached.insert("clamped".into());
                }
            } else {
                reached.borrow_mut().insert(format!("dropped after {:?}", step.window));
            }
            let score = h.engine.accounts()[&a].toxicity_score().raw() * (PER_BP / Fixed::ONE.raw());
            if (score - expected).abs() > TOLERANCE {
                return Err(TestCaseError::fail(format!(
                    "{step:?} after {fill:?}: score {score} fbps, expected {expected}"
                )));
            }
        }
        Ok(())
    });
    if let Err(failure) = result {
        panic!("{failure}");
    }
    let windows = ["Fresh", "Tick", "OneSource", "Divergent"];
    let expected = windows.iter().flat_map(|w| [format!("scored after {w}"), format!("dropped after {w}")]);
    let expected: BTreeSet<String> =
        expected.chain(["clamped", "at memory", "below memory"].map(String::from)).collect();
    let reached = reached.into_inner();
    let missed: Vec<&String> = expected.difference(&reached).collect();
    assert!(missed.is_empty(), "generated steps never reach {missed:?}");
}

// ── Helpers ──

/// A fill, its move to the index that scores it, and what comes first. Sizes run from one unit of
/// precision to 0.5 BTC, a notional of up to about 50,000; moves mostly stay within 50 bps and
/// sometimes pass the 1,000 bps clamp; a delay of more than a horizon drops the markout.
fn step() -> impl Strategy<Value = Step> {
    let side = any::<bool>().prop_map(|buy| if buy { Side::Buy } else { Side::Sell });
    let qty = (1i128..=500, 0u32..=6).prop_map(|(mantissa, exp)| Fixed::from_raw(mantissa * 10i128.pow(exp)));
    let qty = prop_oneof![qty, (1i128..=500_000_000).prop_map(Fixed::from_raw)];
    let move_cbps = prop_oneof![6 => -5_000i128..=5_000, 2 => -100_000i128..=100_000, 1 => -150_000i128..=150_000];
    let delay = || prop_oneof![3 => 1i64..=HORIZON_MS, 1 => HORIZON_MS + 1..=2 * HORIZON_MS];
    let window = prop_oneof![
        2 => (Just(Window::Fresh), prop_oneof![Just(0i64), delay()]),
        1 => (Just(Window::Tick), delay()),
        1 => (Just(Window::OneSource), delay()),
        1 => (Just(Window::Divergent), prop_oneof![Just(0i64), delay()]),
    ];
    (side, qty, move_cbps, window).prop_map(|(side, qty, move_cbps, (window, delay_ms))| Step {
        side,
        qty,
        move_cbps,
        window,
        delay_ms,
    })
}

/// The markout of a fill at `price` against `index`, in femto-bps, clamped to 1,000 bps.
fn markout(price: Fixed, index: Fixed, side: Side) -> i128 {
    let bps = (index.raw() - price.raw()) * i128::from(side.sign()) * 10_000 * PER_BP / price.raw();
    bps.clamp(-1_000 * PER_BP, 1_000 * PER_BP)
}
