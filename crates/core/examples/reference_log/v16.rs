//! The session's `V16` part: lowering the toxicity memory clamps the history each account's score
//! weighs, so the next markout already weighs no more than the new memory.

use engine_core::command::AdminCommand;
use engine_core::params::{NotionalWeighting, ProtocolParams, ToxicityParams};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Side;

use crate::common::stress::with_divergence_shares;
use crate::common::{BTC, Harness, f, params};

/// The part under 100,000 of memory, then 10,000: a 10,000 BTC fill marked out 50 bps up weighs
/// the account's inherited history with its new notional; the lowered memory clamps every
/// history to 10,000, and the next fill's markout weighs the clamped history.
pub(crate) fn session(h: &mut Harness) {
    let set = |memory: &str| {
        let base = with_divergence_shares(params());
        let notional = Some(NotionalWeighting { prior: f("10000"), memory: f(memory) });
        let toxicity = ToxicityParams { notional, ..base.toxicity };
        AdminCommand::SetParams(ProtocolParams { toxicity, ..base })
    };
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V16 });
    h.operator_admin(set("100000"));
    let u = h.signer(33);
    h.deposit(u, "100000");
    h.price(BTC, "100000");
    h.market_order(u, BTC, Side::Buy, "0.1", "100000");
    h.advance(1_000);
    h.price(BTC, "100500");
    h.operator_admin(set("10000"));
    h.market_order(u, BTC, Side::Buy, "0.1", "100500");
    h.advance(1_000);
    h.price(BTC, "100500");
}
