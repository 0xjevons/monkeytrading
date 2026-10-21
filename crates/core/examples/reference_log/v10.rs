//! The session's `V10` part: spreads, a margin requirement, and close-out levels that round up.

use engine_core::command::AdminCommand;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::types::{AssetClass, MarketId, Side};

use crate::common::{Harness, market};

/// A market the session lists to pay spreads that round up.
const DOT: MarketId = 13;
/// A market the session lists to require margin that rounds up.
const ADA: MarketId = 14;
/// A market the session lists at a price of one unit of precision.
const DOGE: MarketId = 15;

/// Activates `V10` at the next seq. In a new market, a buy and a sell pay the 5bps stress spread of
/// 50.0000617283945 rounded up to 50.000061729, and 30 units through the 2bps band pay 400 over
/// 30, rounded up to 13.333333334 a unit. In another, a 0.5 buy at 100.000000001 needs margin of
/// 5.00000000005, rounded up past the account's 5, and is refused; then a long marked down to
/// equity of 500 against margin of 1,000.000000001 is closed out at the 50% level rounded up. In a
/// third, a long bought at two units of precision is halved under stress and closed out at one,
/// as its 5bps spread rounds up to the whole index and stops a unit short of it.
pub(crate) fn session(h: &mut Harness) {
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V10 });
    let [buyer, seller, grower, shorter, thin, marked, dusty] = [19, 20, 21, 22, 23, 24, 25].map(|seed| h.signer(seed));
    for account in [buyer, seller, grower, shorter] {
        h.deposit(account, "1000000");
    }
    let crypto = |id, symbol| market(id, symbol, AssetClass::Crypto, Schedule::AlwaysOpen, 1_000);
    h.operator_admin(AdminCommand::UpsertMarket(crypto(DOT, "DOT-USD")));
    h.operator_admin(AdminCommand::SetStressFloor { market: Some(DOT), level: 1 });
    h.price(DOT, "100000.123456789");
    h.market_order(buyer, DOT, Side::Buy, "0.01", "100000.123456789");
    h.market_order(seller, DOT, Side::Sell, "0.01", "100000.123456789");
    h.operator_admin(AdminCommand::SetStressFloor { market: Some(DOT), level: 0 });
    h.price(DOT, "100000");
    h.market_order(grower, DOT, Side::Buy, "30", "100000");
    h.market_order(shorter, DOT, Side::Sell, "30", "100000");
    h.operator_admin(AdminCommand::UpsertMarket(crypto(ADA, "ADA-USD")));
    h.price(ADA, "100.000000001");
    h.deposit(thin, "5");
    h.market_order(thin, ADA, Side::Buy, "0.5", "100.000000001");
    h.deposit(marked, "10000");
    // Past the stress move window, so the move to the long's entry raises no spread.
    h.advance(61_000);
    h.price(ADA, "19500.00000001");
    h.market_order(marked, ADA, Side::Buy, "1", "19500.00000001");
    h.price(ADA, "10000.00000001");
    h.operator_admin(AdminCommand::UpsertMarket(crypto(DOGE, "DOGE-USD")));
    h.deposit(dusty, "1");
    h.price(DOGE, "0.000000002");
    h.market_order(dusty, DOGE, Side::Buy, "1000000000", "0.000000002");
    h.price(DOGE, "0.000000001");
}
