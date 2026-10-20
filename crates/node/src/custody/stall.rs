//! What a poll of the vault did: read a range, waited for finality, or stopped short, and the
//! alert each kind of stall raises.

use tokio::sync::mpsc;
use tokio::time::{Instant, sleep};
use tracing::{debug, error, info, warn};

use crate::custody::evm::{DepositWatcher, VaultReader};
use crate::sequencer::Request;

/// What a poll that did not stop short did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Polled {
    /// It read a block range; `caught_up` when that range reached the final block.
    Read { caught_up: bool },
    /// No final block has reached the cursor yet.
    Waiting,
}

/// An uninterrupted run of stalled polls.
struct StallEpisode {
    /// The alerts raised so far, so each kind of stall alerts once.
    raised: Vec<&'static str>,
    since: Instant,
    escalated: bool,
}

/// Why a poll stopped short.
pub(super) enum PollError {
    /// The next poll retries.
    Stalled(Stall),
    /// The sequencer or the cursor file failed, so the node must stop.
    Fatal(anyhow::Error),
}

/// A poll that read nothing it could act on. Nothing is credited or skipped until it clears.
pub(super) enum Stall {
    /// The chain could not be read.
    Rpc(anyhow::Error),
    /// The chain's answers disagree in a way a lagging backend explains, so a retry may settle it.
    Unsettled(anyhow::Error),
    /// The chain contradicts the configuration or the cursor; no retry is likely to fix it.
    Inconsistent(anyhow::Error),
}

impl PollError {
    /// A chain read that failed.
    pub(super) fn rpc(e: anyhow::Error) -> Self {
        PollError::Stalled(Stall::Rpc(e))
    }

    /// Chain answers that a lagging backend could explain.
    pub(super) fn unsettled(e: anyhow::Error) -> Self {
        PollError::Stalled(Stall::Unsettled(e))
    }

    /// Chain answers that contradict the configuration or the cursor.
    pub(super) fn inconsistent(e: anyhow::Error) -> Self {
        PollError::Stalled(Stall::Inconsistent(e))
    }
}

impl Stall {
    /// What stopped the poll.
    pub(super) fn error(&self) -> &anyhow::Error {
        match self {
            Stall::Rpc(e) | Stall::Unsettled(e) | Stall::Inconsistent(e) => e,
        }
    }

    /// The alert this stall raises, so a repeat of the same stall stays quiet.
    pub(super) fn alert(&self) -> &'static str {
        match self {
            Stall::Rpc(_) => "custody_rpc_failed",
            Stall::Unsettled(_) => "custody_chain_unsettled",
            Stall::Inconsistent(_) => "custody_chain_inconsistent",
        }
    }

    /// Raises the alert: a Warning when a retry may clear it (a stall that lasts escalates
    /// separately), Critical when it contradicts the configuration or the cursor.
    pub(super) fn raise(&self, next_block: u64) {
        match self {
            Stall::Rpc(e) | Stall::Unsettled(e) => warn!(alert = self.alert(), next_block, "deposits paused: {e:#}"),
            Stall::Inconsistent(e) => error!(alert = self.alert(), next_block, "deposits paused: {e:#}"),
        }
    }
}

impl Polled {
    /// Whether the watcher has nothing left to read: it reached the final block, or waits on it.
    #[cfg(test)]
    pub(super) fn caught_up(self) -> bool {
        match self {
            Polled::Read { caught_up } => caught_up,
            Polled::Waiting => true,
        }
    }
}

impl<V: VaultReader> DepositWatcher<V> {
    /// Polls until a fatal error. A stalled poll raises its alert once per kind of stall in an
    /// episode and is retried every poll interval; a stall that outlasts `stall_critical_after`
    /// raises a Critical alert once.
    /// Error cases:
    /// - the sequencer has stopped, or the cursor cannot be stored
    pub async fn run(mut self, tx: mpsc::Sender<Request>) -> anyhow::Result<()> {
        let mut stalled: Option<StallEpisode> = None;
        loop {
            let next_block = self.cursor.next_block;
            let caught_up = match self.poll(&tx).await {
                Ok(Polled::Read { caught_up }) => {
                    if let Some(episode) = stalled.take() {
                        info!(next_block, stalled_for = ?episode.since.elapsed(), "deposit watcher resumed");
                    }
                    caught_up
                }
                // Waiting reads nothing, so it neither ends a stall nor stops its clock.
                Ok(Polled::Waiting) => true,
                Err(PollError::Fatal(e)) => return Err(e),
                Err(PollError::Stalled(stall)) => {
                    let episode = stalled.get_or_insert(StallEpisode {
                        raised: Vec::new(),
                        since: Instant::now(),
                        escalated: false,
                    });
                    if episode.raised.contains(&stall.alert()) {
                        debug!(next_block, "deposit watcher still stalled: {:#}", stall.error());
                    } else {
                        stall.raise(next_block);
                        episode.raised.push(stall.alert());
                    }
                    let stalled_for = episode.since.elapsed();
                    if !episode.escalated && stalled_for >= self.stall_critical_after {
                        let cause = stall.error();
                        error!(
                            alert = "custody_deposits_stalled",
                            next_block,
                            ?stalled_for,
                            "no vault deposit read: {cause:#}"
                        );
                        episode.escalated = true;
                    }
                    true
                }
            };
            if caught_up {
                sleep(self.poll_interval).await;
            }
        }
    }
}
