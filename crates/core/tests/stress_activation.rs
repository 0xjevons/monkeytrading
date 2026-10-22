//! Activating `V13`: divergence triggers take the form the version in force evaluates, absolute
//! triggers become shares of the widest listed bound, and the move window carries on from the move
//! reference the earlier rules kept, end to end through `Engine::apply`.

mod common;

use common::stress::{BOUNDED, bounded, diverge, with_divergence_shares};
use common::{BTC, Harness, params};
use engine_core::command::AdminCommand;
use engine_core::event::Event;
use engine_core::params::{ProtocolParams, StressLevel, StressTrigger};
use engine_core::protocol::ProtocolVersion;

#[test]
fn divergence_triggers_take_the_form_the_version_evaluates() {
    let share = with_divergence_shares(params());
    let cases = [
        (Harness::v12(), share.clone(), "divergence_share_bps needs protocol V13"),
        (Harness::v13(), params(), "from protocol V13 a divergence trigger is divergence_share_bps"),
    ];
    for (mut h, refused, reason) in cases {
        let ev = h.operate(AdminCommand::SetParams(refused));
        assert_eq!(ev, [Event::AdminRejected { reason: reason.into() }]);
    }
    // A share above the bound could never fire.
    let mut h = Harness::v13();
    for (bps, applied) in [(10_000, true), (10_001, false), (-1, false)] {
        let mut p = share.clone();
        p.stress_levels[1].trigger.as_mut().expect("level 1 has a trigger").divergence_share_bps = Some(bps);
        let ev = h.operate(AdminCommand::SetParams(p));
        assert_eq!(ev.contains(&Event::AdminApplied), applied, "{bps}: {ev:?}");
    }
}

#[test]
fn activating_v13_turns_absolute_divergence_triggers_into_shares_of_the_widest_bound() {
    // Thresholds, a market listed besides the harness's 50bps ones, and the shares after activation:
    // a threshold within the widest bound becomes its share of it, and one past it the bound itself.
    let cases = [
        (vec![10], None, vec![2_000]),
        (vec![30, 40], Some(5), vec![6_000, 8_000]),
        (vec![10], Some(100), vec![1_000]),
        (vec![0, 60], None, vec![0, 10_000]),
    ];
    for (thresholds, listed, shares) in cases {
        let mut h = Harness::v12();
        let mut absolute = ProtocolParams { stress_cooldown_ms: 0, ..params() };
        absolute.stress_levels.truncate(1);
        absolute.stress_levels.extend(thresholds.iter().map(|bps| StressLevel {
            size_spread_multiplier_bps: 10_000,
            min_spread_bps: 5,
            trigger: Some(StressTrigger { divergence_bps: Some(*bps), ..StressTrigger::default() }),
        }));
        h.operator_admin(AdminCommand::SetParams(absolute));
        if let Some(bound) = listed {
            h.operator_admin(AdminCommand::UpsertMarket(bounded(bound)));
        }
        // Before V13 a 5bps bound leaves thresholds of 30 and 40bps out of reach.
        if listed == Some(5) {
            diverge(&mut h, 5);
            assert_eq!(h.engine.markets()[&BOUNDED].stress.level, 0);
        }

        h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V13 });
        let triggers: Vec<StressTrigger> =
            h.engine.params().expect("set").stress_levels.iter().filter_map(|level| level.trigger.clone()).collect();
        let expected: Vec<StressTrigger> = shares
            .iter()
            .map(|share| StressTrigger { divergence_share_bps: Some(*share), ..StressTrigger::default() })
            .collect();
        assert_eq!(triggers, expected, "{thresholds:?}");
        if listed == Some(5) {
            let levels: Vec<usize> = [2, 3, 4]
                .iter()
                .map(|bps| {
                    diverge(&mut h, *bps);
                    h.engine.markets()[&BOUNDED].stress.level
                })
                .collect();
            assert_eq!(levels, [0, 1, 2]);
        }
    }
}

#[test]
fn a_move_begun_before_activation_counts_after_it() {
    // A price under V12, then V13 while its quotes are stale: the V12 move reference starts the window,
    // so 3% down a second later, within the minute, raises the level as V12 would have. With no earlier
    // price there is no move.
    for (seeded, raised) in [(true, true), (false, false)] {
        let mut h = Harness::v12();
        if seeded {
            h.price(BTC, "100000");
        }
        h.advance(2_000);
        h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V13 });
        h.advance(1_000);
        let ev = h.price(BTC, "97000");
        assert_eq!(ev.contains(&Event::StressChanged { market: BTC, level: 1 }), raised, "{seeded}: {ev:?}");
    }
}
