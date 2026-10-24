//! Bounded schedule invalidation, independent of publisher downloads and their signed submission.

use std::time::Duration;

use anyhow::Context;
use hyper::Method;
use tokio::sync::watch;
use tokio::time::sleep;
use tracing::{info, warn};

use crate::admin::{NodeApi, call};
use crate::benchmarks::BenchmarkSchedule;

/// Reads the current durable revision without scanning the public log.
/// Error cases:
/// - the pinned node cannot answer with its current schedule within the call budget
pub(super) async fn current(node: &NodeApi, after: Option<u64>) -> anyhow::Result<BenchmarkSchedule> {
    let path = after.map_or_else(
        || "/v1/benchmarks/schedule".to_owned(),
        |revision| format!("/v1/benchmarks/schedule?after={revision}"),
    );
    call(node, Method::GET, &path, None).await
}

/// Coalesces accepted schedule changes while the worker fetches or finishes an in-flight POST.
/// Reconnection refreshes the authoritative revision before waiting again, covering query gaps.
/// Error cases:
/// - the owning worker drops its receiver
pub(super) async fn run(
    node: &NodeApi,
    initial: Option<u64>,
    retry: Duration,
    updates: watch::Sender<Option<u64>>,
) -> anyhow::Result<()> {
    let mut after = initial;
    let mut healthy = initial.is_some();
    loop {
        match current(node, after).await {
            Ok(schedule) => {
                if !healthy {
                    info!(alert_resolved = "benchmark_schedule_watch_failed", "benchmark schedule watch reconnected");
                    healthy = true;
                }
                if after != Some(schedule.revision) {
                    updates.send(Some(schedule.revision)).context("benchmark schedule consumer stopped")?;
                }
                after = Some(schedule.revision);
            }
            Err(err) => {
                warn!(alert = "benchmark_schedule_watch_failed", "benchmark schedule watch will reconnect: {err:#}");
                healthy = false;
                after = None;
                sleep(retry).await;
            }
        }
    }
}
