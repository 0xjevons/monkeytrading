use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::command::OrderKind;
use crate::types::{MarketId, OrderId, Side};

/// One netted position per market, as on a CFD netting account. `qty` is
/// signed: long > 0, short < 0.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub qty: Fixed,
    pub entry: Fixed,
}

impl Position {
    /// Applies a signed fill and returns the price PnL it realises.
    pub fn apply(&mut self, delta: Fixed, price: Fixed) -> Fixed {
        if delta.is_zero() {
            return Fixed::ZERO;
        }
        if self.qty.is_zero() || self.qty.signum() == delta.signum() {
            let new_qty = self.qty + delta;
            self.entry = (self.entry.mul(self.qty.abs()) + price.mul(delta.abs())).div(new_qty.abs());
            self.qty = new_qty;
            return Fixed::ZERO;
        }
        let closing = delta.abs().min(self.qty.abs());
        let pnl = (price - self.entry).mul(closing).mul_int(self.qty.signum());
        let new_qty = self.qty + delta;
        if new_qty.is_zero() {
            *self = Position::default();
        } else if new_qty.signum() != self.qty.signum() {
            *self = Position { qty: new_qty, entry: price };
        } else {
            self.qty = new_qty;
        }
        pnl
    }

    pub fn unrealised(&self, mark: Fixed) -> Fixed {
        (mark - self.entry).mul(self.qty)
    }

    /// `entry * qty`, kept per market in aggregate so house NAV is O(markets).
    pub fn cost(&self) -> Fixed {
        self.entry.mul(self.qty)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RestingOrder {
    pub order_id: OrderId,
    pub client_id: u64,
    pub market: MarketId,
    pub side: Side,
    pub qty: Fixed,
    pub kind: OrderKind,
    pub reduce_only: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Account {
    /// Cash: deposits - withdrawals + realised PnL - fees - financing.
    pub balance: Fixed,
    pub positions: BTreeMap<MarketId, Position>,
    pub orders: BTreeMap<OrderId, RestingOrder>,
    /// EWMA of fill markouts in bps; see `ToxicityParams`.
    pub toxicity_bps: i64,
    pub withdrawal_nonce: u64,
}

impl Account {
    pub fn position(&self, market: MarketId) -> Position {
        self.positions.get(&market).copied().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(s: &str) -> Fixed {
        s.parse().unwrap()
    }

    #[test]
    fn netting_averages_entry_and_realises_on_reduce() {
        let mut p = Position::default();
        assert_eq!(p.apply(f("1"), f("100")), Fixed::ZERO);
        assert_eq!(p.apply(f("1"), f("110")), Fixed::ZERO);
        assert_eq!(p.entry, f("105"));
        assert_eq!(p.apply(f("-1"), f("120")), f("15"));
        assert_eq!(p, Position { qty: f("1"), entry: f("105") });
        assert_eq!(p.unrealised(f("100")), f("-5"));
    }

    #[test]
    fn flip_closes_then_opens_at_fill_price() {
        let mut p = Position { qty: f("2"), entry: f("100") };
        assert_eq!(p.apply(f("-5"), f("90")), f("-20"));
        assert_eq!(p, Position { qty: f("-3"), entry: f("90") });
        assert_eq!(p.apply(f("3"), f("80")), f("30"));
        assert_eq!(p, Position::default());
    }
}
