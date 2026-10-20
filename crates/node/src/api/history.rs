//! `GET /v1/accounts/{address}/history`: an account's events from the history index, newest first,
//! a page at a time. History is public, like the log it is derived from.

use axum::Json;
use axum::extract::{Path, Query, State};
use engine_core::types::AccountId;
use serde::Deserialize;
use tracing::field::display;
use tracing::{Span, error};

use crate::api::{ApiError, ApiResult, ApiState};
use crate::history::{KINDS, Page, PageQuery};
use crate::log::Offset;

/// Events per page when the request sets no limit.
const DEFAULT_LIMIT: usize = 50;
/// Most events per page, which bounds what one request reads.
pub const MAX_LIMIT: usize = 200;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryParams {
    /// Only events of this kind: an event `type`.
    kind: Option<String>,
    /// Only events before this cursor: the previous page's `next`.
    before: Option<Offset>,
    /// Only events after this cursor.
    after: Option<Offset>,
    limit: Option<usize>,
}

/// One page of an account's events, newest first, strictly between the cursors `after` and
/// `before`.
/// Error cases:
/// - the address is malformed, the kind unknown, or the limit outside 1 to `MAX_LIMIT`
/// - the index cannot be read
pub async fn history(
    State(s): State<ApiState>,
    Path(address): Path<String>,
    Query(params): Query<HistoryParams>,
) -> ApiResult<Page> {
    let account: AccountId = address.parse().map_err(|e| ApiError::BadRequest(format!("{e}")))?;
    Span::current().record("id", display(account));
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(ApiError::BadRequest(format!("limit must be from 1 to {MAX_LIMIT}")));
    }
    if let Some(kind) = &params.kind
        && !KINDS.contains(&kind.as_str())
    {
        return Err(ApiError::BadRequest(format!("unknown kind {kind:?}; expected one of {}", KINDS.join(", "))));
    }
    let query = PageQuery { account, kind: params.kind, before: params.before, after: params.after, limit };
    let page = s.history.page(query).await.map_err(|e| {
        error!(alert = "history_unreadable", "{e:#}");
        ApiError::Internal
    })?;
    Ok(Json(page))
}
