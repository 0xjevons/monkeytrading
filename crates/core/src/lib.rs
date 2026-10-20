//! Deterministic core of the house: a single-threaded state machine that turns sequenced commands
//! into events.
//!
//! The protocol:
//! - The house takes the other side of every trade. There is no order book and there are no
//!   external LPs. House exposure is capped per market and in total as a fraction of house NAV.
//! - Fills happen at once at a multi-venue index mid. Normal size pays no spread. Very large
//!   positions pay fixed-bps size spreads, which widen under stress.
//! - Losers pay nothing; winners pay a flat fee on realised profit. Positions held overnight pay
//!   or receive CFD financing.
//! - Margin is shared across the account at professional-client leverage, with close-out at 50% and
//!   negative balance protection.
//! - RWA markets keep venue hours; nothing fills while a market is closed.

pub mod account;
mod admin;
mod closeout;
pub mod command;
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
pub mod session;
pub mod types;
