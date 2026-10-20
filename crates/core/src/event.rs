//! Engine outputs and the typed reasons a command is refused.

use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::price::PriceError;
use crate::types::{AccountId, MarketId, OrderId, Side};

/// Engine outputs. Clients and the custody layer act on events; they never
/// read state directly.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Deposited {
        account: AccountId,
        amount: Fixed,
    },
    /// A deposit the engine refused to credit: out of range, or a reference already credited.
    DepositRejected {
        account: AccountId,
        amount: Fixed,
        chain_ref: String,
        reason: Reject,
    },
    WithdrawalApproved {
        account: AccountId,
        amount: Fixed,
        nonce: u64,
    },
    WithdrawalRejected {
        account: AccountId,
        amount: Fixed,
        reason: Reject,
    },
    HouseCapitalMoved {
        amount: Fixed,
        nav: Fixed,
    },
    OrderAccepted {
        account: AccountId,
        client_id: u64,
        order_id: OrderId,
    },
    OrderRejected {
        account: AccountId,
        client_id: u64,
        reason: Reject,
    },
    OrderCancelled {
        account: AccountId,
        order_id: OrderId,
        reason: CancelReason,
    },
    Fill(Fill),
    CloseOut {
        account: AccountId,
        equity: Fixed,
        margin: Fixed,
    },
    /// Loss beyond the account's balance, absorbed by the house
    /// (negative balance protection).
    BadDebt {
        account: AccountId,
        amount: Fixed,
    },
    Financing {
        account: AccountId,
        market: MarketId,
        amount: Fixed,
    },
    MarketOpened {
        market: MarketId,
    },
    MarketClosed {
        market: MarketId,
    },
    StressChanged {
        market: MarketId,
        level: usize,
    },
    /// A price update the engine ignored: an unknown market, or a malformed venue quote.
    PricesRejected {
        market: MarketId,
        reason: String,
    },
    AdminApplied,
    AdminRejected {
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reject {
    UnknownMarket,
    UnknownOrder,
    InvalidOrder {
        reason: String,
    },
    MarketClosed,
    ReduceOnly,
    NoPrice(PriceError),
    /// The live price is further from the client's quote than allowed.
    Slippage {
        quoted: Fixed,
        live: Fixed,
        slippage_bps: i64,
    },
    InsufficientMargin {
        equity: Fixed,
        required: Fixed,
    },
    InsufficientFunds {
        available: Fixed,
    },
    HouseCapacity,
    /// An amount at or below zero, or above `Fixed::MAX_INPUT`.
    InvalidAmount,
    /// A custody reference that was already credited.
    DuplicateReference,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelReason {
    Client,
    /// Triggered but couldn't fill (margin, house capacity, or reduce-only
    /// with no position left).
    Unfillable,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Fill {
    pub account: AccountId,
    pub order_id: OrderId,
    pub market: MarketId,
    pub side: Side,
    pub qty: Fixed,
    pub price: Fixed,
    /// Reference mid at fill time. `price` minus `index` is the spread paid.
    pub index: Fixed,
    pub spread_bps: i64,
    /// Price PnL realised by this fill, before the fee.
    pub realised_pnl: Fixed,
    pub profit_fee: Fixed,
    /// Signed position after the fill.
    pub position: Fixed,
    pub liquidation: bool,
}
