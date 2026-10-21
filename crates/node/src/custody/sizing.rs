//! Read sizes that adapt to an RPC provider's limits: a failed read halves the size, and good
//! reads grow it back only up to the size that last worked, so a lasting limit is not hit again
//! on every other read. After a run of good reads that ceiling is raised again, so a transient
//! error does not shrink reads for good.

/// Good reads in a row after which the ceiling is raised.
const RAISE_CEILING_AFTER: u32 = 10;

pub(super) struct AdaptiveSize {
    size: u64,
    ceiling: u64,
    max: u64,
    good_reads: u32,
}

impl AdaptiveSize {
    /// A size that starts at, and never exceeds, `max`.
    pub(super) fn new(max: u64) -> Self {
        AdaptiveSize { size: max, ceiling: max, max, good_reads: 0 }
    }

    /// The size to read next.
    pub(super) fn get(&self) -> u64 {
        self.size
    }

    /// A read of the current size failed.
    pub(super) fn failed(&mut self) {
        self.size = (self.size / 2).max(1);
        self.ceiling = self.size;
        self.good_reads = 0;
    }

    /// A read of the current size answered.
    pub(super) fn succeeded(&mut self) {
        self.good_reads += 1;
        if self.good_reads >= RAISE_CEILING_AFTER && self.ceiling < self.max {
            self.ceiling = self.ceiling.saturating_mul(2).min(self.max);
            self.good_reads = 0;
        }
        self.size = self.size.saturating_mul(2).min(self.ceiling);
    }

    /// Forgets the limit a failure set, once the reads it slowed are done.
    pub(super) fn reset_ceiling(&mut self) {
        self.ceiling = self.max;
    }
}
