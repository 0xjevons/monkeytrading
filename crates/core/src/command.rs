//! Engine inputs. Everything that can change state is a [`Command`], and
//! every command is stamped and sequenced before the engine sees it. The
//! sequenced stream *is* the public event log; replaying it through this
//! crate reproduces the state exactly.

use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::market::MarketSpec;
use crate::params::ProtocolParams;
use crate::price::SourceQuote;
use crate::types::{AccountId, MarketId, Millis, OrderId, Side};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Sequenced {
    pub seq: u64,
    pub ts: Millis,
    pub cmd: Command,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    /// Venue quotes from the feed handlers.
    Prices {
        market: MarketId,
        quotes: Vec<SourceQuote>,
    },
    /// Advances time when nothing else is happening, so session changes and
    /// rollovers fire on schedule.
    Tick,
    /// Credited once the custody chain finalises a vault deposit.
    Deposit {
        account: AccountId,
        amount: Fixed,
        chain_ref: String,
    },
    /// A trader asks to withdraw. If approved, the enclave signs a
    /// withdrawal the vault contract will honour.
    Withdraw {
        account: AccountId,
        amount: Fixed,
    },
    /// House capital in or out. Equity issuance against it is not designed
    /// yet; for now this only moves house NAV.
    HouseCapital {
        amount: Fixed,
        chain_ref: String,
    },
    PlaceOrder(OrderRequest),
    CancelOrder {
        account: AccountId,
        order_id: OrderId,
    },
    Admin(AdminCommand),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OrderRequest {
    pub account: AccountId,
    /// Client-chosen id, echoed back so clients can match responses.
    pub client_id: u64,
    pub market: MarketId,
    pub side: Side,
    pub qty: Fixed,
    pub kind: OrderKind,
    /// Only reduce an existing position, never open or flip it.
    #[serde(default)]
    pub reduce_only: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OrderKind {
    /// Fill now at the live price. `quoted` is the price the client saw.
    /// Quotes are indicative, not firm: the fill happens at the live price,
    /// and the order is declined only if that is more than
    /// `max_slippage_bps` worse than `quoted`. Better prices pass through.
    Market {
        quoted: Fixed,
        #[serde(default)]
        max_slippage_bps: Option<i64>,
    },
    /// Rests until the live price is at `price` or better, then fills at the
    /// live price.
    Limit { price: Fixed },
    /// Rests until the live price trades through `price`, then fills at the
    /// live price, gap included. Not guaranteed.
    Stop { price: Fixed },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum AdminCommand {
    SetParams(ProtocolParams),
    /// Lists a new market or replaces an existing market's spec.
    UpsertMarket(MarketSpec),
    SetBenchmarkRate {
        benchmark: String,
        annual_bps: i64,
    },
    /// The operator's stress floor for one market, or every market if
    /// `market` is `None`. The effective level is the higher of this and the
    /// automatic level, so the operator can raise stress but never suppress it.
    SetStressFloor {
        market: Option<MarketId>,
        level: usize,
    },
    /// Stops new risk in a market. Reducing trades stay allowed.
    SetReduceOnly {
        market: MarketId,
        reduce_only: bool,
    },
}
