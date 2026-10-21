//! The reference session's `V13` part: stress that follows its inputs.

use engine_core::command::{AdminCommand, Command, OrderKind};
use engine_core::market::{MarketSpec, PriceRules};
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::types::{AssetClass, MarketId, Side};

use crate::SOL;
use crate::common::stress::with_divergence_shares;
use crate::common::{BTC, Harness, f, market, params};

/// A market the `V13` part lists with a house cap floored at 100,000.
const LINK: MarketId = 19;
/// A market the `V13` part lists with a 5bps divergence bound.
const GBP: MarketId = 20;

/// Activates `V13`, then sends divergence triggers refused in each way, stress raised by fills (one
/// within the command of the fill before it), a move straddling two windows, a divergence trigger
/// on a 5bps bound, a cooldown that ends on a tick, and a move on a window's edge.
pub(super) fn session(h: &mut Harness) {
    // V13 activates at this seq, after house capital back in so only the markets below come near a
    // trigger, and turns the 10bps divergence trigger into 20% of the widest bound, 50bps. A
    // divergence trigger is now a share of each market's bound: an absolute one is refused, and so
    // are a share past the bound and a listing with a zero bound, where no share above zero fires.
    h.advance(1_000);
    h.cmd(Command::HouseCapital { amount: f("100000000"), chain_ref: "0xcap:6".into() });
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V13 });
    h.operate(AdminCommand::SetParams(params()));
    let mut beyond = with_divergence_shares(params());
    beyond.stress_levels[1].trigger.as_mut().expect("level 1 has a trigger").divergence_share_bps = Some(10_001);
    h.operate(AdminCommand::SetParams(beyond));
    h.operator_admin(AdminCommand::SetParams(with_divergence_shares(params())));
    let crypto = |id, symbol| market(id, symbol, AssetClass::Crypto, Schedule::AlwaysOpen, 1_000);
    let agreed = crypto(LINK, "LINK-USD");
    let price_rules = PriceRules { max_divergence_bps: 0, ..agreed.price_rules };
    h.operate(AdminCommand::UpsertMarket(MarketSpec { price_rules, ..agreed }));

    // Stress after fills: two buy stops one price triggers, the first taking the house past 80% of a
    // 100,000 cap, so the second, and the market order after it, pay the 5bps level.
    let capped = MarketSpec { house_cap_bps: 1, house_cap_floor: f("100000"), ..crypto(LINK, "LINK-USD") };
    h.operator_admin(AdminCommand::UpsertMarket(capped));
    let (p, r) = (h.signer(31), h.signer(32));
    h.deposit(p, "20000");
    h.deposit(r, "5000");
    h.price(LINK, "99");
    h.order(p, LINK, Side::Buy, "850", OrderKind::Stop { price: f("100") });
    h.order(r, LINK, Side::Buy, "10", OrderKind::Stop { price: f("100") });
    h.price(LINK, "100");
    h.market_order(r, LINK, Side::Buy, "10", "100");

    // A move over a trailing window: 3% down 20s after a price that itself came 50s after the first.
    h.advance(1_000);
    h.price(SOL, "150.5");
    h.advance(50_000);
    h.price(SOL, "150.5");
    h.advance(20_000);
    h.price(SOL, "146");

    // A 1bps divergence on a 5bps bound meets 20% of it; calm quotes inside the cooldown hold the
    // level, and the tick once it has passed steps it down.
    let fx = market(GBP, "GBP-USD", AssetClass::Fx, Schedule::AlwaysOpen, 50);
    let price_rules = PriceRules { max_divergence_bps: 5, ..fx.price_rules };
    h.operator_admin(AdminCommand::UpsertMarket(MarketSpec { price_rules, ..fx }));
    h.quotes(GBP, &["1.30", "1.30014"]);
    h.advance(299_500);
    h.price(GBP, "1.30");
    h.advance(600);
    h.cmd(Command::Tick);

    // BTC, calm since the `V12` part, 3% down a whole window after its last price.
    h.price(BTC, "80000");
    h.advance(60_000);
    h.price(BTC, "77600");
}
