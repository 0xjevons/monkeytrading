//! `/v1/archive/{file}`: the log's completed segments and their manifest, for anyone to download
//! and check with `engine replay --log <dir>`. The manifest comes from enclave memory, not the
//! host's copy on disk, so a downloader can check each segment file against it.

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use axum::response::{IntoResponse, Response};
use tokio::fs::File;
use tokio_util::io::ReaderStream;
use tracing::field::display;
use tracing::{Span, error};

use crate::api::{ApiError, ApiState};
use crate::log::{MANIFEST, segment_seq};

const JSON_LINES: &str = "application/x-ndjson";

/// Serves `manifest.jsonl`, or a completed segment it lists, from the log directory. The segment
/// still being written is not served; the stream covers it.
/// Error cases:
/// - the file is neither the manifest nor a listed segment
/// - the manifest cannot be encoded, or the segment cannot be opened
pub async fn archive(State(s): State<ApiState>, Path(file): Path<String>) -> Result<Response, ApiError> {
    Span::current().record("id", display(&file));
    if file == MANIFEST {
        let manifest = s.manifest.borrow().clone();
        let body = manifest.to_jsonl().map_err(|e| {
            error!(alert = "archive_unreadable", "encoding manifest: {e:#}");
            ApiError::Internal
        })?;
        return Ok(([(CONTENT_TYPE, JSON_LINES)], body).into_response());
    }
    let listed = segment_seq(&file).and_then(|seq| s.manifest.borrow().get(seq).cloned()).ok_or(ApiError::NotFound)?;
    let segment = File::open(s.log_dir.join(&listed.file)).await.map_err(|e| {
        error!(alert = "archive_unreadable", "opening segment: {e}");
        ApiError::Internal
    })?;
    let headers = [(CONTENT_TYPE, JSON_LINES.to_owned()), (CONTENT_LENGTH, listed.bytes.to_string())];
    Ok((headers, Body::from_stream(ReaderStream::new(segment))).into_response())
}
