//! The session's `V15` part: an order that can only reduce the account's position in its market
//! skips the minimum notional at placement.

use engine_core::command::{AdminCommand, OrderKind};
use engine_core::event::{CancelReason, Event, Reject};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Side;

use crate::common::{ETH, Harness, f};
use crate::helpers::accepted;

/// `V15` activates. ETH's minimum notional is 100 (set in the main session). A trader long 0.1
/// rests a plain sell below the minimum, a plain sell that would flip is refused, a marketable
/// reducing sell fills below the minimum, and a plain stop that rested as a reduction is
/// cancelled once an earlier fill leaves it adding risk; an account with no position is refused
/// below the minimum.
pub(crate) fn session(h: &mut Harness) {
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V15 });
    let (x, y) = (h.signer(33), h.signer(34));
    h.deposit(x, "10000");
    h.deposit(y, "10000");
    h.price(ETH, "3000");
    h.market_order(x, ETH, Side::Buy, "0.1", "3000");
    // A plain sell of 0.02 at 3,200 is 64, below the minimum, but it can only reduce the long.
    h.order(x, ETH, Side::Sell, "0.02", OrderKind::Limit { price: f("3200") });
    // A plain sell of 0.2 would flip the long and, at 80 below the minimum, is refused.
    let events = h.order(x, ETH, Side::Sell, "0.2", OrderKind::Limit { price: f("400") });
    assert!(matches!(&events[..], [Event::OrderRejected { reason: Reject::InvalidOrder { .. }, .. }]), "{events:?}");
    // A reducing sell marketable at placement fills below the minimum.
    let events = h.order(x, ETH, Side::Sell, "0.01", OrderKind::Limit { price: f("3000") });
    let filled = accepted(&events);
    assert!(events.iter().any(|event| matches!(event, Event::Fill(fill) if fill.order_id == filled)), "{events:?}");
    // A plain stop of 0.03 at 2,900 is 87, below the minimum, and rests as a reduction.
    let stop = accepted(&h.order(x, ETH, Side::Sell, "0.03", OrderKind::Stop { price: f("2900") }));
    // An earlier fill shrinks the long to 0.01, so the stop would now flip: triggered, it is
    // cancelled rather than filled.
    h.market_order(x, ETH, Side::Sell, "0.08", "3000");
    let events = h.price(ETH, "2890");
    assert!(events.contains(&Event::OrderCancelled { account: x, order_id: stop, reason: CancelReason::Unfillable }));
    // An account with no position cannot skip the minimum.
    let events = h.order(y, ETH, Side::Sell, "0.02", OrderKind::Limit { price: f("3200") });
    assert!(matches!(&events[..], [Event::OrderRejected { reason: Reject::InvalidOrder { .. }, .. }]), "{events:?}");
}
