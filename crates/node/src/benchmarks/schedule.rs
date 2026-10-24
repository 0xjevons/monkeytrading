//! Compact durable rollover notifications for the external publisher worker.

use engine_core::Engine;
use engine_core::params::Rollover;
use serde::{Deserialize, Serialize};

/// A rollover schedule and the durable log revision at which it was published.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkSchedule {
    pub revision: u64,
    pub rollover: Option<Rollover>,
}

impl BenchmarkSchedule {
    /// Reads the schedule without cloning publisher history or scanning markets.
    pub(crate) fn from_engine(engine: &Engine) -> Self {
        Self {
            revision: engine.last_seq().map_or(0, |seq| seq + 1),
            rollover: engine.params().map(|params| params.rollover.clone()),
        }
    }
}
