//! `/v1/stream`: the public log over a WebSocket. A subscriber that names an offset gets the log
//! from there, read from disk, then live messages, each once and in offset order. A subscriber
//! that falls behind the live channel reads from disk again from its last offset instead of being
//! dropped.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::Response;
use serde::Deserialize;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, watch};
use tokio::task::spawn_blocking;
use tracing::{Instrument, Span, error};

use crate::api::{ApiError, ApiState};
use crate::log::{LogReader, Manifest, Offset};
use crate::sequencer::StreamMsg;

/// Most messages read from disk per blocking call.
const BACKFILL_BATCH: usize = 256;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamParams {
    /// Start right after this offset: the last one the subscriber received.
    after: Option<Offset>,
    /// Start at the first message of this seq; `0` is the whole log.
    from: Option<u64>,
}

/// One subscriber's messages, in offset order, each once.
struct Feed {
    live: broadcast::Receiver<Arc<StreamMsg>>,
    log_dir: PathBuf,
    manifest: watch::Receiver<Manifest>,
    /// The lowest offset not yet yielded; `None` until a live-only feed yields its first message.
    next: Option<Offset>,
    /// Reading from disk until it reaches what has been written.
    catching_up: bool,
    reader: Option<LogReader>,
    /// Read from disk, not yet yielded.
    backlog: VecDeque<Arc<StreamMsg>>,
}

impl Feed {
    /// A feed that starts at `start`, or with the next live message if `None`. `live` must be
    /// subscribed before the feed first reads the disk, so that every message is either on disk by
    /// then or still to come live.
    fn new(
        live: broadcast::Receiver<Arc<StreamMsg>>,
        log_dir: PathBuf,
        manifest: watch::Receiver<Manifest>,
        start: Option<Offset>,
    ) -> Self {
        let backlog = VecDeque::new();
        Self { live, log_dir, manifest, next: start, catching_up: start.is_some(), reader: None, backlog }
    }

    /// The next message, or `None` once the sequencer has stopped.
    /// Error cases:
    /// - the log on disk cannot be read (see `LogReader`)
    async fn next(&mut self) -> anyhow::Result<Option<Arc<StreamMsg>>> {
        loop {
            if let Some(msg) = self.backlog.pop_front() {
                self.next = Some(msg.offset.next());
                return Ok(Some(msg));
            }
            if self.catching_up {
                let from = self.next.context("catching up with no offset to start from")?;
                let (dir, manifest, reader) = (self.log_dir.clone(), self.manifest.clone(), self.reader.take());
                let (reader, batch) = spawn_blocking(move || read_batch(&dir, manifest, from, reader))
                    .await
                    .context("log reader task")??;
                self.catching_up = reader.is_some();
                self.reader = reader;
                self.backlog.extend(batch);
                continue;
            }
            match self.live.recv().await {
                // Already yielded from disk.
                Ok(msg) if self.next.is_some_and(|next| msg.offset < next) => {}
                Ok(msg) => {
                    self.next = Some(msg.offset.next());
                    return Ok(Some(msg));
                }
                // What the channel dropped is on disk. A live-only feed that has yielded nothing yet
                // has no gap: it starts at whatever the channel still holds.
                Err(RecvError::Lagged(_)) => self.catching_up = self.next.is_some(),
                Err(RecvError::Closed) => return Ok(None),
            }
        }
    }
}

/// Upgrades to a WebSocket that streams the public log: live from now by default, or from `after`
/// or `from` through what is on disk and then live.
/// Error cases:
/// - both `after` and `from` are given
pub async fn stream(
    State(s): State<ApiState>,
    Query(params): Query<StreamParams>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let start = match (params.after, params.from) {
        (Some(_), Some(_)) => return Err(ApiError::BadRequest("give after or from, not both".into())),
        (Some(after), None) => Some(after.next()),
        (None, Some(seq)) => Some(Offset::start_of(seq)),
        (None, None) => None,
    };
    let feed = Feed::new(s.stream.subscribe(), s.log_dir.clone(), s.manifest.clone(), start);
    let span = Span::current();
    Ok(ws.on_upgrade(move |socket| forward(socket, feed).instrument(span)))
}

/// Sends the feed to one subscriber until it disconnects or the sequencer stops.
async fn forward(mut socket: WebSocket, mut feed: Feed) {
    loop {
        let msg = match feed.next().await {
            Ok(Some(msg)) => msg,
            Ok(None) => return,
            Err(e) => {
                error!(alert = "stream_backfill_failed", "{e:#}");
                return;
            }
        };
        // Closing rather than skipping keeps the subscriber's view gap-free; it can resume.
        let json = match serde_json::to_string(&*msg) {
            Ok(json) => json,
            Err(e) => {
                error!(alert = "stream_encode_failed", offset = %msg.offset, "{e}");
                return;
            }
        };
        if socket.send(Message::Text(json.into())).await.is_err() {
            return;
        }
    }
}

/// Up to `BACKFILL_BATCH` messages from disk, opening a reader at `from` when there is none yet,
/// and the reader to go on with, or `None` once it has read everything written.
/// Error cases:
/// - the log cannot be read (see `LogReader`)
fn read_batch(
    dir: &Path,
    manifest: watch::Receiver<Manifest>,
    from: Offset,
    reader: Option<LogReader>,
) -> anyhow::Result<(Option<LogReader>, Vec<Arc<StreamMsg>>)> {
    let mut reader = match reader {
        Some(reader) => reader,
        None => LogReader::open(dir, from, manifest)?,
    };
    let mut batch = Vec::new();
    while batch.len() < BACKFILL_BATCH {
        let Some((offset, entry)) = reader.next_entry()? else {
            return Ok((None, batch));
        };
        batch.push(Arc::new(StreamMsg::new(offset, entry)));
    }
    Ok((Some(reader), batch))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use engine_core::command::Command;
    use tokio::time::timeout;

    use crate::test_support::{TestSequencer, deposit, log_messages, scratch_dir, withdraw};

    #[tokio::test]
    async fn a_feed_from_an_offset_yields_the_log_then_live_messages_once_each() {
        let dir = scratch_dir("feed-from");
        let node = TestSequencer::start(&dir, 1024);
        node.submit(deposit(100, "dep-1")).await;
        let mut feed =
            Feed::new(node.stream.subscribe(), dir.clone(), node.manifest.clone(), Some(Offset::start_of(0)));
        // These reach both the disk and the live channel before the feed reads either.
        node.submit(withdraw(40)).await;
        for n in 0..5 {
            node.submit(deposit(1, &format!("dep-{n}"))).await;
        }
        let expected = log_messages(&dir, Offset::start_of(0));
        assert_eq!(take(&mut feed, expected.len()).await, expected);
        assert_next_is_the_next_command(&node, &mut feed).await;
    }

    #[tokio::test]
    async fn a_feed_resumed_between_two_messages_of_one_seq_starts_at_the_second() {
        let dir = scratch_dir("feed-mid-seq");
        let node = TestSequencer::start(&dir, 1024);
        node.submit(deposit(100, "dep-1")).await;
        let seq = node.tick_to_checkpoint().await;
        node.submit(withdraw(40)).await;
        let after = Offset::start_of(seq);
        let mut feed = Feed::new(node.stream.subscribe(), dir.clone(), node.manifest.clone(), Some(after.next()));

        let expected = log_messages(&dir, after.next());
        let got = take(&mut feed, expected.len()).await;
        assert!(got[0].starts_with(&format!(r#"{{"offset":"{seq}:1","type":"withdrawal""#)), "{}", got[0]);
        assert_eq!(got, expected);
        assert_next_is_the_next_command(&node, &mut feed).await;
    }

    #[tokio::test]
    async fn a_feed_that_falls_behind_the_live_channel_catches_up_from_disk() {
        let dir = scratch_dir("feed-lag");
        let node = TestSequencer::start(&dir, 2);
        let mut feed = Feed::new(node.stream.subscribe(), dir.clone(), node.manifest.clone(), None);
        node.submit(deposit(100, "dep-1")).await;
        let first = next_msg(&mut feed).await;
        for n in 0..20 {
            node.submit(deposit(1, &format!("dep-{n}"))).await;
        }
        let expected = log_messages(&dir, first.offset.next());
        assert!(expected.len() > 2);
        assert_eq!(take(&mut feed, expected.len()).await, expected);
        assert_next_is_the_next_command(&node, &mut feed).await;
    }

    #[tokio::test]
    async fn a_live_feed_starts_at_the_next_message_and_one_that_lags_before_it_starts_later() {
        let dir = scratch_dir("feed-live");
        let node = TestSequencer::start(&dir, 2);
        let mut feed = Feed::new(node.stream.subscribe(), dir.clone(), node.manifest.clone(), None);
        assert_next_is_the_next_command(&node, &mut feed).await;

        let mut feed = Feed::new(node.stream.subscribe(), dir.clone(), node.manifest.clone(), None);
        for n in 0..10 {
            node.submit(deposit(1, &format!("dep-{n}"))).await;
        }
        let first = next_msg(&mut feed).await;
        let expected = log_messages(&dir, first.offset.next());
        assert_eq!(take(&mut feed, expected.len()).await, expected);
    }

    // ── Helpers ──

    /// The feed's next message, failing the test if none comes within ten seconds.
    async fn next_msg(feed: &mut Feed) -> Arc<StreamMsg> {
        let next = timeout(Duration::from_secs(10), feed.next()).await.expect("a message in time");
        next.unwrap().expect("sequencer running")
    }

    /// The next `n` messages, as the stream encodes them.
    async fn take(feed: &mut Feed, n: usize) -> Vec<String> {
        let mut got = Vec::with_capacity(n);
        while got.len() < n {
            got.push(serde_json::to_string(&*next_msg(feed).await).unwrap());
        }
        got
    }

    /// Sequences a tick and checks it is the feed's next message, so nothing before it repeats.
    async fn assert_next_is_the_next_command(node: &TestSequencer, feed: &mut Feed) {
        let seq = node.next_seq().await;
        node.submit(Command::Tick).await;
        assert_eq!(next_msg(feed).await.offset, Offset::start_of(seq));
    }
}
