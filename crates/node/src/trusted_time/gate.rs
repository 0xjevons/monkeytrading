//! The sequencer's check of each stamp against trusted time.

use std::time::{Duration, Instant};

use engine_core::types::Millis;
use tracing::{debug, error, info};

use crate::trusted_time::{Sample, ceil_millis, floor_millis};

/// How long sequencing may go on after the latest trusted time sample (#15 §4).
pub const NO_SAMPLE_GRACE: Duration = Duration::from_secs(60);

/// Why sequencing is halted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Halt {
    /// Waiting, within the grace, for the run's first sample: a normal start.
    Starting,
    Drift,
    NoSample,
}

/// Why a stamp was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Refusal {
    /// The stamp, or the host clock reading it came from, is not within the drift bound of every
    /// time the sample allows.
    Drift { host_ms: Millis, earliest_ms: Millis, latest_ms: Millis },
    /// No sample for `waited`: none since the start, or the latest is older than the grace.
    NoSample { waited: Duration, sampled: bool },
}

/// Admits a stamp only while it, and the host clock reading it came from, lie within `max_drift`
/// of every time the latest sample allows. Once the latest sample is older than `NO_SAMPLE_GRACE`,
/// nothing passes until a fresh one arrives. The grace of #15 §4 runs from the latest sample of
/// this run, and a run starts with none: nothing passes before its first sample, so a restart,
/// which the host controls, never opens a window on the host clock alone. Waiting for the first
/// sample alerts only once the grace has passed.
pub struct TimeGate {
    max_drift_ms: Millis,
    started: Instant,
    sample: Option<Sample>,
    halted: Option<Halt>,
    /// Stamps refused since sequencing halted.
    refused: u64,
}

impl Refusal {
    /// The halt this refusal puts sequencing in.
    fn halt(self) -> Halt {
        match self {
            Refusal::Drift { .. } => Halt::Drift,
            Refusal::NoSample { waited, sampled: false } if waited <= NO_SAMPLE_GRACE => Halt::Starting,
            Refusal::NoSample { .. } => Halt::NoSample,
        }
    }
}

impl TimeGate {
    /// A gate with no sample yet, started at `now`.
    pub fn new(max_drift: Duration, now: Instant) -> Self {
        Self { max_drift_ms: floor_millis(max_drift), started: now, sample: None, halted: None, refused: 0 }
    }

    /// Replaces the latest sample.
    pub fn update(&mut self, sample: Sample) {
        self.sample = Some(sample);
    }

    /// Whether `ts`, stamped from the host clock reading `host_ms`, may be logged at monotonic time
    /// `now`. Raises a Critical alert when sequencing halts, or halts for another reason, and logs
    /// when it resumes.
    pub fn admit(&mut self, host_ms: Millis, ts: Millis, now: Instant) -> bool {
        let refusal = self.check(host_ms, ts, now);
        let halt = refusal.map(Refusal::halt);
        match refusal {
            None if self.halted == Some(Halt::Starting) => {
                info!(refused = self.refused, "first trusted time sample: sequencing started");
                self.refused = 0;
            }
            None if self.halted.is_some() => {
                info!(refused = self.refused, "stamps are within the drift bound of trusted time: sequencing resumed");
                self.refused = 0;
            }
            None => {}
            Some(r) => {
                if self.halted != halt && halt != Some(Halt::Starting) {
                    self.alert(ts, r);
                }
                debug!(ts, refusal = ?r, "stamp refused: sequencing halted");
                self.refused += 1;
            }
        }
        self.halted = halt;
        refusal.is_none()
    }

    /// Why `ts`, stamped from `host_ms`, may not be logged at `now`, if it may not.
    fn check(&self, host_ms: Millis, ts: Millis, now: Instant) -> Option<Refusal> {
        let Some(sample) = self.sample else {
            return Some(Refusal::NoSample { waited: now.saturating_duration_since(self.started), sampled: false });
        };
        let age = now.saturating_duration_since(sample.at);
        if age > NO_SAMPLE_GRACE {
            return Some(Refusal::NoSample { waited: age, sampled: true });
        }
        // True time has moved on since the sample by the monotonic time elapsed.
        let earliest_ms = sample.earliest_ms.saturating_add(floor_millis(age));
        let latest_ms = sample.latest_ms.saturating_add(ceil_millis(age));
        let within = |t: Millis| {
            latest_ms.saturating_sub(self.max_drift_ms) <= t && t <= earliest_ms.saturating_add(self.max_drift_ms)
        };
        (!(within(host_ms) && within(ts))).then_some(Refusal::Drift { host_ms, earliest_ms, latest_ms })
    }

    /// Raises the Critical alert for a refusal that halts sequencing.
    fn alert(&self, ts: Millis, refusal: Refusal) {
        match refusal {
            Refusal::Drift { host_ms, earliest_ms, latest_ms } => error!(
                alert = "clock_drift",
                ts,
                host = host_ms,
                earliest = earliest_ms,
                latest = latest_ms,
                max_drift_ms = self.max_drift_ms,
                "host clock or stamp is not within the drift bound of trusted time: sequencing halted"
            ),
            Refusal::NoSample { waited, sampled } => error!(
                alert = "trusted_time_unavailable",
                for_ms = floor_millis(waited),
                sampled,
                "no trusted time sample within the grace: sequencing halted"
            ),
        }
    }
}
