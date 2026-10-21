//! The session's `V13` part: a listing refused for a benchmark with no rate, the resting-order cap,
//! the minimum notional at placement, a placement that checks only its own order, and a rollover
//! over a benchmark with no rate.

use engine_core::command::{AdminCommand, Command, OrderKind};
use engine_core::market::{FinancingSpec, MarketSpec};
use engine_core::params::ProtocolParams;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::types::{AssetClass, MarketId, Side};

use crate::common::stress::with_divergence_shares;
use crate::common::{ETH, Harness, f, market, ny, params};
use crate::helpers::accepted;

/// A market the part lists under a benchmark with no rate, before `V13`.
const SILVER: MarketId = 19;
/// A listing `V13` refuses, under the same benchmark.
const PLATINUM: MarketId = 20;
/// SOL, which the session lists earlier, financed at SOFR.
const SOL: MarketId = 10;

/// A silver listing names its benchmark "Sofr", which has no rate, before `V13` activates; under
/// `V13` a platinum listing naming it is refused, and resting orders are capped at two per account.
/// Two orders rest, a third is refused at the cap, a cancel frees a place, and a limit below ETH's
/// minimum notional is refused as it is placed. A limit held back by its spread stays resting while
/// another account places an order, and fills on the next price. At the rollover the silver long
/// accrues nothing and the rollover records "Sofr", while the SOFR positions are financed.
pub(crate) fn session(h: &mut Harness) {
    let listing = |id, symbol| {
        let spec = market(id, symbol, AssetClass::Commodity, Schedule::AlwaysOpen, 1_000);
        MarketSpec { financing: FinancingSpec { benchmark: "Sofr".into(), ..spec.financing.clone() }, ..spec }
    };
    h.operator_admin(AdminCommand::UpsertMarket(listing(SILVER, "XAG-USD")));
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V13 });
    h.operate(AdminCommand::UpsertMarket(listing(PLATINUM, "XPT-USD")));
    let capped = ProtocolParams {
        profit_fee_bps: 1_500,
        house_total_cap_floor: f("10000000"),
        ..with_divergence_shares(params())
    };
    h.operator_admin(AdminCommand::SetParams(ProtocolParams { max_resting_orders: 2, ..capped }));
    let (s, r) = (h.signer(31), h.signer(32));
    h.deposit(s, "10000");
    h.deposit(r, "10000");
    h.price(SILVER, "30");
    h.market_order(s, SILVER, Side::Buy, "100", "30");
    h.price(SOL, "150");
    h.market_order(r, SOL, Side::Buy, "10", "150");
    let first = accepted(&h.order(r, SILVER, Side::Buy, "10", OrderKind::Limit { price: f("20") }));
    h.order(r, ETH, Side::Sell, "0.1", OrderKind::Limit { price: f("4000") });
    h.order(r, SILVER, Side::Buy, "10", OrderKind::Limit { price: f("19") });
    h.cmd(Command::CancelOrder { account: r, order_id: first, auth: None });
    // A sell limit of 0.01 ETH at 4,000 is 40, below ETH's minimum notional of 100.
    h.order(r, ETH, Side::Sell, "0.01", OrderKind::Limit { price: f("4000") });
    // The 5bps stress spread holds back a buy limit at the mid; with the floor back at zero,
    // another account's order leaves it resting, and the next price fills it.
    h.operator_admin(AdminCommand::SetStressFloor { market: Some(SILVER), level: 1 });
    h.price(SILVER, "30");
    h.order(s, SILVER, Side::Buy, "10", OrderKind::Limit { price: f("30") });
    h.operator_admin(AdminCommand::SetStressFloor { market: Some(SILVER), level: 0 });
    h.order(r, SILVER, Side::Buy, "10", OrderKind::Limit { price: f("20") });
    h.price(SILVER, "30");
    h.advance_to(ny(2026, 10, 28, 18, 1));
    h.cmd(Command::Tick);
}
