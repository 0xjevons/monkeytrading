//! The session's `V17` part: the move window survives a halt shorter than it.

use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AssetClass, MarketId};

use crate::common::stress::halted_us_equities;
use crate::common::{Harness, market, ny};

/// A market the part lists for the across-halt move.
const NVDA: MarketId = 21;

/// `V17` activates. A market that halts 20s, 10s after its last price, reopens 3% away, and raises
/// fast-move stress: the pre-halt price is still inside the move window.
pub(crate) fn session(h: &mut Harness) {
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V17 });
    let schedule = halted_us_equities("2026-10-29T13:00:00", "2026-10-29T13:00:20");
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
}
