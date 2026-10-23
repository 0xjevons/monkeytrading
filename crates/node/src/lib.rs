//! The TEE-hosted node around `engine-core`: the sequencer and public log, log replay, the client
//! API with its TLS and limits, enclave attestation, trusted time, custody, price feeds, and the
//! operator's metrics and alert routing; and `engine admin`, the operator's client.

pub mod admin;
mod alerts;
mod api;
mod clock;
pub mod config;
pub use config::NodeConfig;
mod custody;
mod feeds;
mod history;
mod limits;
pub mod log;
mod metrics;
mod monitor;
mod node;
pub use node::run;
mod roughtime;
pub mod sequencer;
pub use sequencer::{replay, replay_path};
mod telemetry;
pub use telemetry::{Telemetry, init_logging, init_telemetry};
mod tee;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod test_tracing;
mod throttle;
mod tls;
mod trusted_time;
