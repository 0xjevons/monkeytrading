//! Durable schedule notifications and publisher synchronization at a changed rollover.

use engine_core::params::Rollover;
use engine_core::types::Millis;
use tracing::warn;

use crate::sequencer::Sequencer;

impl Sequencer {
    /// Marks an accepted schedule change for publication after its log batch is flushed.
    pub(super) fn note_benchmark_schedule(&mut self, previous: Option<Rollover>) {
        if previous == self.engine.params().map(|params| params.rollover.clone()) {
            return;
        }
        self.benchmark_schedule_dirty = true;
        self.benchmark_schedule_pending |= self.benchmark_sync_enabled;
    }

    /// Holds a newly due rollover until a fresh complete signed publisher attempt is accepted.
    pub(super) fn admit_benchmark_schedule(&mut self, ts: Millis) {
        if self.benchmark_schedule_pending
            && !self.benchmark_sync
            && self.engine.next_rollover().is_some_and(|next| next <= ts)
        {
            self.benchmark_sync = true;
            warn!(alert = "benchmark_sync_pending", "waiting for signed publisher history after rollover changes");
        }
    }
}
