//! Watchdogs for an endpoint that answers but stops moving: a head whose timestamp falls behind
//! this node's clock, a final block that stops advancing while the head moves on, and reads that
//! keep trailing the final block. Each leaves new deposits waiting with no failed read to report,
//! so each raises a Critical alert once per episode, keyed by its name alone, and resolves it when
//! the condition clears.

use std::time::Duration;

use tokio::time::Instant;
use tracing::{error, info};

use crate::clock::now_ms;
use crate::custody::evm::HeadSupport;

/// The highest final block seen, the head when it was, and since when the head has moved on
/// without the final block rising.
struct FinalitySeen {
    highest: Option<u64>,
    head: u64,
    heads: Vec<Option<u64>>,
    moving_since: Option<Instant>,
    alerted: bool,
}

pub(super) struct Watchdog {
    budget: Duration,
    /// Since when reads have not reached the final block, and whether that was alerted.
    behind_since: Option<Instant>,
    lag_alerted: bool,
    finality: Option<FinalitySeen>,
    /// The newest head timestamp seen, in seconds, so a stale backend among current ones is
    /// no news.
    newest_head_time: u64,
    head_stale: bool,
}

impl Watchdog {
    /// Watchdogs that alert once a condition lasts `budget`.
    pub(super) fn new(budget: Duration) -> Self {
        Watchdog {
            budget,
            behind_since: None,
            lag_alerted: false,
            finality: None,
            newest_head_time: 0,
            head_stale: false,
        }
    }

    /// Checks one finality read: the head block and its timestamp in seconds, and the final block.
    pub(super) fn observe(
        &mut self,
        head: u64,
        head_time: u64,
        final_block: Option<u64>,
        support: Option<&HeadSupport>,
    ) {
        self.check_head(head, head_time);
        self.check_finality(head, final_block, support);
    }

    /// Checks one range read, or one header batch of a proof that keeps the next read short of
    /// the final block: alerts, once, when reads have trailed the final block for the budget, as a
    /// catch-up slowed to a crawl does with every read succeeding.
    pub(super) fn observe_read(&mut self, caught_up: bool, next_block: u64, final_block: u64) {
        if caught_up {
            if self.lag_alerted {
                info!(
                    alert_resolved = "custody_deposits_lagging",
                    "deposit reads have caught up with the final block at block {next_block}"
                );
            }
            (self.behind_since, self.lag_alerted) = (None, false);
            return;
        }
        let behind_for = self.behind_since.get_or_insert_with(Instant::now).elapsed();
        if !self.lag_alerted && behind_for >= self.budget {
            error!(
                alert = "custody_deposits_lagging",
                "deposit reads trail the final block: at block {next_block} of {final_block} for {behind_for:?}"
            );
            self.lag_alerted = true;
        }
    }

    /// Alerts when the newest head seen is older than the budget by this node's clock: the
    /// endpoint has stopped following the chain. The host sets the clock; this only alerts.
    fn check_head(&mut self, head: u64, head_time: u64) {
        self.newest_head_time = self.newest_head_time.max(head_time);
        let now = Duration::from_millis(u64::try_from(now_ms()).unwrap_or(0));
        let age = now.saturating_sub(Duration::from_secs(self.newest_head_time));
        if age < self.budget {
            if self.head_stale {
                info!(alert_resolved = "custody_head_stale", "the endpoint's head is current again at block {head}");
                self.head_stale = false;
            }
        } else if !self.head_stale {
            error!(alert = "custody_head_stale", "the endpoint's head is not advancing: block {head} is {age:?} old");
            self.head_stale = true;
        }
    }

    /// Alerts when the budget has passed since the head first moved on without the highest final
    /// block rising. Quorum progress needs a configured majority advancing against their own
    /// baselines; scalar bounds may change contributors between polls. A lower final block is
    /// no progress. An idle chain raises nothing and its first majority movement gets the budget.
    fn check_finality(&mut self, head: u64, final_block: Option<u64>, support: Option<&HeadSupport>) {
        let now = Instant::now();
        let rose = self.finality.as_ref().is_none_or(|seen| final_block > seen.highest);
        if rose {
            if self.finality.as_ref().is_some_and(|seen| seen.alerted) {
                info!(
                    alert_resolved = "custody_finality_stalled",
                    "the final block is advancing again: {final_block:?}"
                );
            }
            self.finality = Some(FinalitySeen {
                highest: final_block,
                head,
                heads: support.map(|s| s.heads.clone()).unwrap_or_default(),
                moving_since: None,
                alerted: false,
            });
            return;
        }
        let seen = self.finality.as_mut().expect("set when the final block rose");
        let advanced = if let Some(support) = support {
            // Provider indices belong to one immutable Quorum owned by this watcher.
            assert_eq!(seen.heads.len(), support.heads.len(), "custody provider set is immutable");
            let mut advancing = 0;
            for (baseline, current) in seen.heads.iter_mut().zip(&support.heads) {
                match (*baseline, *current) {
                    (Some(before), Some(now)) if now > before => advancing += 1,
                    (None, Some(now)) => *baseline = Some(now),
                    _ => {}
                }
            }
            advancing >= support.required
        } else {
            head > seen.head
        };
        if advanced && seen.moving_since.is_none() {
            seen.moving_since = Some(now);
        }
        let Some(moving_since) = seen.moving_since else { return };
        let stalled_for = now.duration_since(moving_since);
        if !seen.alerted && stalled_for >= self.budget {
            let highest = seen.highest;
            error!(
                alert = "custody_finality_stalled",
                "the final block has stopped advancing: {highest:?} with the head at {head} for {stalled_for:?}"
            );
            seen.alerted = true;
        }
    }
}
