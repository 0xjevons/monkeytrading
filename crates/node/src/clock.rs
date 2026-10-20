//! The node's clocks. Inside a TEE the host supplies the wall clock, so the sequencer only uses it
//! to stamp commands, checks it against trusted time, and never moves sequenced time backwards.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use engine_core::types::Millis;

/// The clocks the sequencer and the trusted time check read.
pub trait Clock: Send + Sync {
    /// Wall-clock milliseconds since the epoch, as the host reports them.
    fn now_ms(&self) -> Millis;

    /// Monotonic time, for round trips and the age of a trusted time sample.
    ///
    /// TODO: the host can scale the guest's monotonic clock unless its TSC is protected (TDX, or
    /// SEV-SNP with Secure TSC); require that at attestation.
    fn instant(&self) -> Instant;
}

/// The operating system's clocks.
pub struct SystemClock;

impl Clock for SystemClock {
    /// Wall-clock milliseconds since the epoch.
    fn now_ms(&self) -> Millis {
        now_ms()
    }

    /// `Instant::now()`.
    fn instant(&self) -> Instant {
        Instant::now()
    }
}

/// Wall-clock milliseconds since the epoch.
pub fn now_ms() -> Millis {
    // The epoch is in the past on any machine that can run the node.
    SystemTime::now().duration_since(UNIX_EPOCH).expect("clock after epoch").as_millis() as Millis
}

#[cfg(test)]
pub mod test_clock {
    use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use engine_core::types::Millis;

    use crate::clock::{Clock, now_ms};

    /// A host clock that stands still unless a test moves it. True time is the system's at
    /// creation plus `advance`; the wall clock adds `skew` on top, and monotonic time moves with
    /// `advance`.
    pub struct TestClock {
        wall_ms: Millis,
        instant: Instant,
        skew_ms: AtomicI64,
        advanced_ms: AtomicU64,
    }

    impl Default for TestClock {
        /// A clock at the system's current time, unskewed.
        fn default() -> Self {
            Self {
                wall_ms: now_ms(),
                instant: Instant::now(),
                skew_ms: AtomicI64::new(0),
                advanced_ms: AtomicU64::new(0),
            }
        }
    }

    impl TestClock {
        /// A clock whose true time starts at `wall_ms`.
        pub fn at(wall_ms: Millis) -> Self {
            Self { wall_ms, ..Self::default() }
        }

        /// Sets how far the wall clock runs ahead of true time (behind when negative).
        pub fn skew(&self, ms: Millis) {
            self.skew_ms.store(ms, Ordering::SeqCst);
        }

        /// Moves true time, the wall clock, and monotonic time forward by `by`.
        pub fn advance(&self, by: Duration) {
            self.advanced_ms.fetch_add(by.as_millis() as u64, Ordering::SeqCst);
        }

        /// True time: what an honest Roughtime server reports.
        pub fn true_ms(&self) -> Millis {
            self.wall_ms + self.advanced_ms.load(Ordering::SeqCst) as Millis
        }
    }

    impl Clock for TestClock {
        /// True time plus the skew.
        fn now_ms(&self) -> Millis {
            self.true_ms() + self.skew_ms.load(Ordering::SeqCst)
        }

        /// The creation instant plus the advance.
        fn instant(&self) -> Instant {
            self.instant + Duration::from_millis(self.advanced_ms.load(Ordering::SeqCst))
        }
    }
}
