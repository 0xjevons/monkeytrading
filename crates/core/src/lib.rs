//! Deterministic core of the house: a single-threaded state machine that turns sequenced commands
//! into events.
//!
//! The protocol:
//! - The house takes the other side of every trade. There is no order book and there are no
//!   external LPs. House exposure is capped per market and in total as a fraction of house NAV,
//!   from `V6` with a fixed dollar floor under each cap.
//! - Fills happen at once at a multi-venue index mid. Normal size pays no spread. Very large
//!   positions pay fixed-bps size spreads, which widen under stress.
//! - Losers pay nothing; winners pay a flat fee on realised profit. Positions held overnight pay
//!   or receive CFD financing.
//! - Margin is shared across the account at professional-client leverage, with close-out at 50% and
//!   negative balance protection. From `V6`, close-outs also follow financing and parameter or
//!   listing changes.
//! - RWA markets keep venue hours; nothing fills while a market is closed.
//! - Rules are versioned, and a log activates each version at a recorded seq. From `V3`, orders,
//!   cancels, and withdrawals are authorised as the log's client auth records: by the account's
//!   EIP-712 signature, next nonce, and expiry, or, in dev logs only, unsigned. From `V5`, every
//!   rounding in position accounting favours the house, so no sequence of fills realises more than
//!   its exact PnL. From `V8`, the operator signs admin commands as clients sign requests, with the
//!   key the log records, once anything is credited, and one command makes every market
//!   reduce-only.
//! - Accounts hold USD. From `V7`, a market may be quoted in another currency: it accrues PnL in it,
//!   converts to USD at its FX market's index, and has no price while that FX market has none.

pub mod account;
mod admin;
pub mod auth;
mod closeout;
pub mod command;
pub mod eip712;
pub mod engine;
pub use engine::{Engine, SequenceError};
pub mod event;
mod execution;
pub mod fixed;
pub use fixed::Fixed;
mod funds;
pub mod market;
pub mod params;
pub mod price;
pub mod protocol;
pub mod session;
pub mod types;
