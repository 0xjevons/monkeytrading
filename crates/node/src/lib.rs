//! The TEE-hosted node around `engine-core`: the sequencer and public log, log replay, the client
//! API, enclave attestation, trusted time, custody, and price feeds.

mod api;
mod clock;
pub mod config;
pub use config::NodeConfig;
mod custody;
mod feeds;
pub mod log;
mod node;
pub use node::{init_logging, run};
mod roughtime;
pub mod sequencer;
pub use sequencer::replay;
mod tee;
#[cfg(test)]
mod test_support;
mod trusted_time;
