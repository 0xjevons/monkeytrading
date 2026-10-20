//! `/v1/archive/{file}`: the log's completed segments and their manifest, for anyone to download
//! and check with `engine replay --log <dir>`.

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use axum::response::{IntoResponse, Response};
use tokio::task::spawn_blocking;
use tokio_util::io::ReaderStream;
use tracing::Span;
use tracing::error;
use tracing::field::display;

use crate::api::{ApiError, ApiState};
use crate::log::{MANIFEST, Manifest};

/// Serves `manifest.json`, or a completed segment it lists, from the log directory. The segment
/// still being written is not served; the stream covers it.
/// Error cases:
/// - the file is neither the manifest nor a listed segment
/// - the manifest or the segment cannot be read
pub async fn archive(State(s): State<ApiState>, Path(file): Path<String>) -> Result<Response, ApiError> {
    Span::current().record("id", display(&file));
    let dir = s.log_dir.clone();
    let manifest = spawn_blocking(move || Manifest::load(&dir))
        .await
        .map_err(anyhow::Error::from)
        .and_then(|loaded| loaded)
        .map_err(|e| {
            error!(alert = "archive_unreadable", "{e:#}");
            ApiError::Internal
        })?;
    if file == MANIFEST {
        return Ok(Json(manifest).into_response());
    }
    let listed = manifest.segments.into_iter().find(|seg| seg.file == file).ok_or(ApiError::NotFound)?;
    let segment = tokio::fs::File::open(s.log_dir.join(&listed.file)).await.map_err(|e| {
        error!(alert = "archive_unreadable", "opening segment: {e}");
        ApiError::Internal
    })?;
    let headers = [(CONTENT_TYPE, "application/x-ndjson".to_owned()), (CONTENT_LENGTH, listed.bytes.to_string())];
    Ok((headers, Body::from_stream(ReaderStream::new(segment))).into_response())
}
