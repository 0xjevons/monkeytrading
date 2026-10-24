//! Bounded rollover watches share the public API's subscriber cap.

use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;
use tokio::sync::oneshot;
use tokio::time::{Instant, timeout_at};
use tracing::Span;

use crate::admin::CALL_TIMEOUT;
use crate::api::{ApiError, ApiResult, ApiState};
use crate::benchmarks::BenchmarkSchedule;
use crate::sequencer::Request;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ScheduleParams {
    after: Option<u64>,
}

/// Returns the durable schedule immediately, or waits within the node-call transport budget.
/// Error cases:
/// - every subscriber place is held, or the sequencer cannot supply a durable subscription
pub(super) async fn schedule(
    State(state): State<ApiState>,
    Query(params): Query<ScheduleParams>,
) -> ApiResult<BenchmarkSchedule> {
    let _place = state.limits.subscribe()?;
    let deadline = Instant::now() + CALL_TIMEOUT / 2;
    let subscribe = async {
        let (reply, subscribed) = oneshot::channel();
        state.tx.send(Request::WatchBenchmarkSchedule(reply)).await.map_err(|_| ApiError::Unavailable)?;
        subscribed.await.map_err(|_| ApiError::Unavailable)
    };
    let mut updates = timeout_at(deadline, subscribe).await.map_err(|_| ApiError::Unavailable)??;
    let current = updates.borrow_and_update().clone();
    Span::current().record("id", current.revision);
    if params.after == Some(current.revision)
        && let Ok(changed) = timeout_at(deadline, updates.changed()).await
    {
        changed.map_err(|_| ApiError::Unavailable)?;
    }
    Ok(Json(updates.borrow_and_update().clone()))
}

#[cfg(test)]
mod tests;
