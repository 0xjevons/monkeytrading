//! The session's `V11` part: toxicity scores averaged by notional and scored at a valid index.

use engine_core::command::{AdminCommand, Command};
use engine_core::params::{ProtocolParams, ToxicityParams};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Side;

use crate::common::{BTC, Harness, params};

/// Parameters the operator sets without notional weighting keep `V11` from activating until the
/// node gives them its own; then a score averages markouts by notional from 10,000 of neutral
/// history, and parameters without the weighting are refused. Past BTC's stress cooldown, a buy of
/// 20,000 marked out 3 bps up scores 2, and a dust buy marked out beside it 0.000000024. A buy whose
/// quotes are stale at its horizon waits through a tick for the next fresh index, and a sell whose
/// next index comes more than a horizon after its markout falls due is dropped.
pub(crate) fn session(h: &mut Harness) {
    let unweighted = ProtocolParams { toxicity: ToxicityParams { notional: None, ..params().toxicity }, ..params() };
    h.operator_admin(AdminCommand::SetParams(unweighted.clone()));
    let activate = Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V11 });
    h.cmd(activate.clone());
    h.admin(AdminCommand::SetToxicityNotional(params().toxicity.notional.expect("the harness weighs by notional")));
    h.cmd(activate);
    h.operate(AdminCommand::SetParams(unweighted));
    let [edge, duster, waiter] = [26, 27, 28].map(|seed| h.signer(seed));
    [edge, duster, waiter].into_iter().for_each(|account| h.deposit(account, "100000"));
    h.advance(301_000);
    h.price(BTC, "80000");
    h.market_order(edge, BTC, Side::Buy, "0.25", "80000");
    h.market_order(duster, BTC, Side::Buy, "0.000000001", "80000");
    h.advance(1_000);
    h.price(BTC, "80024");
    h.advance(500);
    h.market_order(waiter, BTC, Side::Buy, "0.125", "80024");
    h.advance(1_000);
    h.cmd(Command::Tick);
    h.price(BTC, "80048");
    h.market_order(waiter, BTC, Side::Sell, "0.125", "80048");
    h.advance(2_001);
    h.price(BTC, "80000");
}
