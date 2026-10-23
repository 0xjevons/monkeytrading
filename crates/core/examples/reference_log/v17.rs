//! The session's `V17` part: the move window survives a halt shorter than it, and a market keeps
//! its stress level while its FX pair has no price.

use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::{Period, Schedule};
use engine_core::types::{AssetClass, MarketId};

use crate::common::{DE40, EURUSD, Harness, market, ny, us_equities};

/// A market the part lists for the across-halt move.
const NVDA: MarketId = 21;

/// `V17` activates. A market that halts 20s, 10s after its last price, reopens 3% away, and raises
/// fast-move stress: the pre-halt price is still inside the move window. Germany 40 keeps its
/// level while EUR-USD has no price, stepping down once a fresh EUR-USD index restores the
/// reading.
pub(crate) fn session(h: &mut Harness) {
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V17 });
    let Schedule::Weekly { tz, windows, .. } = us_equities() else { unreachable!("US equities keep weekly hours") };
    let at = |local: &str| local.parse().expect("a local date and time");
    let schedule = Schedule::Weekly {
        tz,
        windows,
        closed_dates: vec![],
        closures: vec![Period { from: at("2026-10-29T13:00:00"), to: at("2026-10-29T13:00:20") }],
        special_sessions: vec![],
    };
    h.operator_admin(AdminCommand::UpsertMarket(market(NVDA, "NVDA", AssetClass::Equity, schedule, 2_000)));

    // The last pre-halt price, 10s before a 20s halt; the reopening price is 3% down, 30s after it.
    h.advance_to(ny(2026, 10, 29, 12, 59) + 50_000);
    h.price(NVDA, "230");
    h.advance_to(ny(2026, 10, 29, 13, 0));
    h.cmd(Command::Tick);
    h.advance(20_000);
    let events = h.price(NVDA, "223.1");
    assert!(events.contains(&Event::MarketOpened { market: NVDA }), "{events:?}");
    assert!(events.contains(&Event::StressChanged { market: NVDA, level: 1 }), "{events:?}");

    // Germany 40, raised by a 3% move, keeps its level while EUR-USD's quotes age out, even past
    // the cooldown; a fresh EUR-USD index restores the reading and steps it down.
    h.price(EURUSD, "1.1");
    h.price(DE40, "20000");
    let events = h.price(DE40, "19400");
    assert!(events.contains(&Event::StressChanged { market: DE40, level: 1 }), "{events:?}");
    h.advance(2_000);
    h.price(DE40, "19400");
    h.advance(300_000);
    h.cmd(Command::Tick);
    assert_eq!(h.engine.markets()[&DE40].stress.level, 1);
    h.price(DE40, "19400");
    let events = h.price(EURUSD, "1.1");
    assert!(events.contains(&Event::StressChanged { market: DE40, level: 0 }), "{events:?}");
}
