//! The key and value hash of each kind of leaf. Amounts and prices are `int128` in `Fixed`'s raw
//! units (9 decimals). Value hashes, in Solidity terms:
//! - **Account:** `keccak256(abi.encode(AccountLeaf))`, the struct below, where `openPositions`
//!   counts the account's position leaves.
//! - **Position:** `keccak256(abi.encode(PositionLeaf))`, one per open position, keyed by its
//!   account and market.
//! - **Market:** `keccak256(abi.encode(MarketLeaf))`, with `marked` false and `mark` 0 before the
//!   market's first price; `quoteCurrency` 0 for USD, 1 for EUR, 2 for GBP, and 3 for JPY, with
//!   `fxMarket` the market whose index converts it to USD (0 for USD); `fxMarked` false and
//!   `fxMark` 0 until that index has priced it; and `stateHash` the keccak-256 of the JSON encoding
//!   of its spec, venue quotes, open and reduce-only flags, and stress state. A market's index of
//!   resting orders is left out: the order leaves hold those orders. An amount in a EUR or GBP
//!   market is `fxMark` times as many USD, and one in a JPY market `1 / fxMark` times as many.
//! - **Resting order:** the keccak-256 of the JSON encoding of `{ "account", "order" }`.
//! - **Request nonce:** `keccak256(abi.encode(uint64 nonce))`, the account's next nonce.
//! - **Credited reference:** `keccak256(bytes(reference))`.
//! - **Markout:** the keccak-256 of its JSON encoding.
//! - **Remaining state:** the keccak-256 of the JSON encoding of every engine field no other leaf
//!   holds (`state.rs`).
//!
//! ```solidity
//! struct AccountLeaf { int128 balance; uint64 withdrawalNonce; int64 toxicityBps; uint32 openPositions; }
//! struct PositionLeaf { int128 qty; int128 entry; }
//! struct MarketLeaf {
//!     bool marked; int128 mark; uint8 quoteCurrency; uint32 fxMarket; bool fxMarked; int128 fxMark;
//!     int128 clientQty; int128 clientCost; bytes32 stateHash;
//! }
//! ```
//!
//! No leaf grows with what a client accumulates: each position and resting order has a leaf of
//! its own. A market's quotes are keyed by the node's own feed sources, and its spec and the
//! remaining state are set by operator commands. JSON encodings are the ones the node's log uses
//! for the same types.

use std::collections::BTreeMap;

use alloy_primitives::B256;
use alloy_sol_types::SolValue;
use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::account::{Account, Position, RestingOrder};
use crate::engine::{MarketState, Markout, StressState};
use crate::market::{MarketSpec, QuoteCurrency};
use crate::merkle::keccak;
use crate::price::SourceQuote;
use crate::types::{AccountId, MarketId, OrderId};

/// The kind byte that starts each key.
const GLOBALS: u8 = 0;
const ACCOUNT: u8 = 1;
const MARKET: u8 = 2;
const REQUEST_NONCE: u8 = 3;
const CREDITED_REF: u8 = 4;
const MARKOUT: u8 = 5;
const RESTING_ORDER: u8 = 6;
const POSITION: u8 = 7;

/// The structs whose ABI encodings account, position, and market leaves hash.
mod typed {
    use alloy_sol_types::sol;

    sol! {
        struct AccountLeaf {
            int128 balance;
            uint64 withdrawalNonce;
            int64 toxicityBps;
            uint32 openPositions;
        }

        struct PositionLeaf {
            int128 qty;
            int128 entry;
        }

        struct MarketLeaf {
            bool marked;
            int128 mark;
            uint8 quoteCurrency;
            uint32 fxMarket;
            bool fxMarked;
            int128 fxMark;
            int128 clientQty;
            int128 clientCost;
            bytes32 stateHash;
        }
    }
}

/// An account as its leaf holds it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountLeaf {
    pub balance: Fixed,
    pub withdrawal_nonce: u64,
    pub toxicity_bps: i64,
    /// How many positions the account holds, each in a leaf of its own.
    pub open_positions: u32,
}

/// A market as its leaf holds it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarketLeaf {
    /// The mark positions are valued at, once the market has been priced.
    pub mark: Option<Fixed>,
    /// The currency the market is quoted in, and the market whose index converts it to USD.
    pub quote_currency: QuoteCurrency,
    /// The index amounts convert to USD at, once that market has priced it.
    pub fx_mark: Option<Fixed>,
    pub client_qty: Fixed,
    pub client_cost: Fixed,
    /// keccak-256 of the JSON encoding of the market's spec, venue quotes, open and reduce-only
    /// flags, and stress state.
    pub state_hash: B256,
}

/// What a market's `state_hash` covers.
#[derive(Serialize)]
struct MarketRest<'a> {
    spec: &'a MarketSpec,
    quotes: &'a BTreeMap<String, SourceQuote>,
    open: bool,
    reduce_only: bool,
    stress: &'a StressState,
}

/// What a resting order's leaf covers.
#[derive(Serialize)]
struct OrderRest<'a> {
    account: AccountId,
    order: &'a RestingOrder,
}

impl AccountLeaf {
    /// The leaf of `account`.
    pub(crate) fn of(account: &Account) -> Self {
        // Every field is named, so a field added to `Account` fails to compile until a leaf holds it.
        let Account { balance, positions, orders: _, toxicity_bps, withdrawal_nonce } = account;
        AccountLeaf {
            balance: *balance,
            withdrawal_nonce: *withdrawal_nonce,
            toxicity_bps: *toxicity_bps,
            // Positions are keyed by `u32` market ids, so their count fits a `u32`.
            open_positions: u32::try_from(positions.len()).expect("one position per market id"),
        }
    }

    /// The leaf's value hash.
    pub fn value_hash(&self) -> B256 {
        let leaf = typed::AccountLeaf {
            balance: self.balance.raw(),
            withdrawalNonce: self.withdrawal_nonce,
            toxicityBps: self.toxicity_bps,
            openPositions: self.open_positions,
        };
        keccak(&[&leaf.abi_encode()])
    }
}

impl MarketLeaf {
    /// The leaf of `market`.
    pub(crate) fn of(market: &MarketState) -> Self {
        // `resting` indexes orders the order leaves hold.
        let MarketState { spec, quotes, open, reduce_only, mark, fx_mark, client_qty, client_cost, stress, resting: _ } =
            market;
        let rest = MarketRest { spec, quotes, open: *open, reduce_only: *reduce_only, stress };
        MarketLeaf {
            mark: *mark,
            quote_currency: spec.quote_currency,
            fx_mark: *fx_mark,
            client_qty: *client_qty,
            client_cost: *client_cost,
            state_hash: json_hash(&rest),
        }
    }

    /// The leaf's value hash.
    pub fn value_hash(&self) -> B256 {
        let (quote_currency, fx_market) = match self.quote_currency {
            QuoteCurrency::Usd => (0, 0),
            QuoteCurrency::Eur { fx_market } => (1, fx_market),
            QuoteCurrency::Gbp { fx_market } => (2, fx_market),
            QuoteCurrency::Jpy { fx_market } => (3, fx_market),
        };
        let leaf = typed::MarketLeaf {
            marked: self.mark.is_some(),
            mark: self.mark.unwrap_or(Fixed::ZERO).raw(),
            quoteCurrency: quote_currency,
            fxMarket: fx_market,
            fxMarked: self.fx_mark.is_some(),
            fxMark: self.fx_mark.unwrap_or(Fixed::ZERO).raw(),
            clientQty: self.client_qty.raw(),
            clientCost: self.client_cost.raw(),
            stateHash: self.state_hash,
        };
        keccak(&[&leaf.abi_encode()])
    }
}

/// The value hash of a position's leaf.
pub fn position_hash(position: &Position) -> B256 {
    // Every field is named, so a field added to `Position` fails to compile until the leaf holds it.
    let Position { qty, entry } = position;
    keccak(&[&typed::PositionLeaf { qty: qty.raw(), entry: entry.raw() }.abi_encode()])
}

/// The key of the leaf holding the engine's remaining state.
pub(crate) fn globals_key() -> B256 {
    keccak(&[&[GLOBALS]])
}

/// The key of `account`'s leaf.
pub fn account_key(account: AccountId) -> B256 {
    keccak(&[&[ACCOUNT], &account.0])
}

/// The key of `account`'s position in `market`.
pub fn position_key(account: AccountId, market: MarketId) -> B256 {
    keccak(&[&[POSITION], &account.0, &market.to_be_bytes()])
}

/// The key of `market`'s leaf.
pub fn market_key(market: MarketId) -> B256 {
    keccak(&[&[MARKET], &market.to_be_bytes()])
}

/// The key of a resting order's leaf.
pub(crate) fn order_key(order_id: OrderId) -> B256 {
    keccak(&[&[RESTING_ORDER], &order_id.to_be_bytes()])
}

/// The key of `account`'s request nonce leaf.
pub(crate) fn request_nonce_key(account: AccountId) -> B256 {
    keccak(&[&[REQUEST_NONCE], &account.0])
}

/// The key of a credited custody reference's leaf.
pub(crate) fn credited_ref_key(chain_ref: &str) -> B256 {
    keccak(&[&[CREDITED_REF], chain_ref.as_bytes()])
}

/// The key of the markout at `index`.
pub(crate) fn markout_key(index: u64) -> B256 {
    keccak(&[&[MARKOUT], &index.to_be_bytes()])
}

/// The value hash of `account`'s resting order.
pub(crate) fn order_hash(account: AccountId, order: &RestingOrder) -> B256 {
    json_hash(&OrderRest { account, order })
}

/// The value hash of a request nonce leaf.
pub(crate) fn request_nonce_hash(nonce: u64) -> B256 {
    keccak(&[&nonce.abi_encode()])
}

/// The value hash of a credited reference's leaf.
pub(crate) fn credited_ref_hash(chain_ref: &str) -> B256 {
    keccak(&[chain_ref.as_bytes()])
}

/// The value hash of a markout's leaf.
pub(crate) fn markout_hash(markout: &Markout) -> B256 {
    json_hash(markout)
}

/// keccak-256 of `value`'s JSON encoding.
pub(crate) fn json_hash(value: &impl Serialize) -> B256 {
    // Engine state is maps, lists, strings, and integers, all of which serde_json encodes.
    keccak(&[&serde_json::to_vec(value).expect("engine state serialises")])
}
