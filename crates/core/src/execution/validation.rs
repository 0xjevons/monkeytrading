//! The checks an order passes before it is considered at all.

use crate::Fixed;
use crate::command::{OrderKind, OrderRequest};
use crate::engine::Engine;
use crate::event::Reject;
use crate::fixed::Rounding;
use crate::protocol::ProtocolVersion;

impl Engine {
    /// Checks an order can be considered at all.
    /// Error cases:
    /// - protocol params not set yet
    /// - an unknown market
    /// - a qty, quoted price, or trigger price at or below zero or above `Fixed::MAX_INPUT`
    /// - an account that has never been credited
    /// - from `V14`, a limit or stop order from an account already resting the parameters' cap, or
    ///   one that is not reduce-only whose notional at its own price, in USD at that price in a
    ///   market that converts at its own index and at its FX market's last mark in any other, is
    ///   below the market's minimum, or whose FX market has no mark yet
    pub(super) fn validate_order(&self, req: &OrderRequest) -> Result<(), Reject> {
        let invalid = |reason: &str| Err(Reject::InvalidOrder { reason: reason.into() });
        let Some(params) = &self.params else {
            return invalid("protocol params not set");
        };
        let Some(market) = self.markets.get(&req.market) else {
            return Err(Reject::UnknownMarket);
        };
        if !req.qty.is_valid_input() {
            return invalid("qty out of range");
        }
        let (price, rests) = match req.kind {
            OrderKind::Market { quoted, .. } => (quoted, false),
            OrderKind::Limit { price } | OrderKind::Stop { price } => (price, true),
        };
        if !price.is_valid_input() {
            return invalid("price out of range");
        }
        let Some(acct) = self.accounts.get(&req.account) else {
            return Err(Reject::InsufficientFunds { available: Fixed::ZERO });
        };
        if rests && self.protocol_version >= ProtocolVersion::V14 {
            if acct.orders.len() >= params.max_resting_orders {
                return Err(Reject::TooManyOrders { max: params.max_resting_orders });
            }
            // A reduce-only order never adds risk, as a reducing fill pays no minimum. The order is
            // valued at its own price, converted as its fill would be: a market that converts at its
            // own index, as USD-JPY does, at that price, and any other at its FX market's last mark.
            if !req.reduce_only {
                let currency = market.spec.quote_currency;
                // Listing checks the FX market is listed, and no market is ever removed.
                let fx_at = |fx_market| {
                    if fx_market == req.market {
                        Ok(price)
                    } else {
                        self.markets[&fx_market].mark.ok_or(Reject::NoFxPrice { fx_market })
                    }
                };
                let fx = currency.fx_market().map(fx_at).transpose()?;
                if currency.to_usd(req.qty.mul(price), fx, Rounding::Down) < market.spec.min_order_notional {
                    return invalid("below minimum order notional");
                }
            }
        }
        Ok(())
    }
}
