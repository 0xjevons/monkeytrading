use super::*;
use std::fs;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use futures_util::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use crate::log::{MANIFEST, Manifest, Offset, SegmentedLog, segment_seq};
use crate::sequencer::replay;
use crate::tee::{EnclaveKey, MockAttestor, attest_key};
use crate::test_support::{SEGMENT_COMMANDS, TestSequencer, deposit, dev_config, log_messages, scratch_dir, withdraw};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// The feature's dev scenario: a subscriber reads from offset zero, disconnects between two
/// messages of one seq, resumes after the last offset it received, and sees every message once;
/// then the archive downloads and replays.
#[tokio::test]
async fn dev_scenario_resume_between_messages_of_one_seq_and_replay_the_archive() {
    let dir = scratch_dir("dev-scenario");
    let node = TestSequencer::start(&dir, 1024);
    let addr = serve(&node, &dir).await;
    node.submit(deposit(100, "dep-1")).await;
    let seq = node.tick_to_checkpoint().await;
    node.submit(withdraw(40)).await;

    let mut received: Vec<String> = Vec::new();
    let mut ws = connect(addr, "from=0").await;
    while received.last().is_none_or(|m| offset(m) != Offset::start_of(seq)) {
        received.push(next_text(&mut ws).await);
    }
    // The withdrawal and checkpoint of `seq` were sent but never read.
    drop(ws);

    node.submit(withdraw(10)).await;
    for n in 0..SEGMENT_COMMANDS {
        node.submit(deposit(1, &format!("dep-offline-{n}"))).await;
    }
    let mut ws = connect(addr, &format!("after={seq}:0")).await;
    for n in 0..SEGMENT_COMMANDS {
        node.submit(deposit(1, &format!("dep-live-{n}"))).await;
    }
    let expected = log_messages(&dir, Offset::start_of(0));
    while received.len() < expected.len() {
        received.push(next_text(&mut ws).await);
    }
    assert_eq!(received, expected);

    let (status, body) = get(addr, &format!("/v1/archive/{MANIFEST}")).await;
    assert_eq!(status, 200);
    let archive = scratch_dir("dev-scenario-archive");
    fs::create_dir_all(&archive).unwrap();
    fs::write(archive.join(MANIFEST), &body).unwrap();
    let manifest = Manifest::load(&archive).unwrap();
    assert!(manifest.segments.len() >= 3, "{manifest:?}");
    for listed in &manifest.segments {
        let (status, segment) = get(addr, &format!("/v1/archive/{}", listed.file)).await;
        assert_eq!(status, 200);
        fs::write(archive.join(&listed.file), segment).unwrap();
    }
    let replayed = replay(&SegmentedLog::open(&archive).unwrap().entries).unwrap();
    assert_eq!(replayed.engine.last_seq(), manifest.segments.last().map(|s| s.last_seq));
    assert!(replayed.checkpoints > 0);
}

#[tokio::test]
async fn the_archive_serves_the_enclave_manifest_and_completed_segments_only() {
    let dir = scratch_dir("archive-routes");
    let node = TestSequencer::start(&dir, 16);
    let addr = serve(&node, &dir).await;
    for n in 0..2 * SEGMENT_COMMANDS {
        node.submit(deposit(1, &format!("dep-{n}"))).await;
    }
    let manifest = node.manifest.borrow().clone();
    let completed = &manifest.segments[0];
    let (status, body) = get(addr, &format!("/v1/archive/{}", completed.file)).await;
    assert_eq!((status, body), (200, fs::read(dir.join(&completed.file)).unwrap()));

    // The host's copy of the manifest is not what gets served or trusted.
    let listed = fs::read(dir.join(MANIFEST)).unwrap();
    let forged = br#"{"file":"/etc/hosts","first_seq":999,"last_seq":999,"bytes":1,"sha256":"00"}"#;
    fs::write(dir.join(MANIFEST), [listed.as_slice(), forged, b"\n"].concat()).unwrap();
    assert_eq!(get(addr, &format!("/v1/archive/{MANIFEST}")).await, (200, listed));

    let open_segment = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .find(|name| segment_seq(name).is_some_and(|seq| manifest.get(seq).is_none()))
        .unwrap();
    for path in
        [format!("/v1/archive/{open_segment}"), "/v1/archive/nothing.jsonl".into(), "/v1/archive/%2Fetc%2Fhosts".into()]
    {
        assert_eq!(get(addr, &path).await.0, 404, "{path}");
    }

    fs::remove_file(dir.join(&manifest.segments[1].file)).unwrap();
    assert_eq!(get(addr, &format!("/v1/archive/{}", manifest.segments[1].file)).await.0, 500);
}

#[tokio::test]
async fn the_archive_serves_only_what_the_enclave_manifest_lists() {
    let dir = scratch_dir("archive-empty");
    let node = TestSequencer::start(&dir, 16);
    // Segment 0 is complete on disk, but this manifest lists nothing.
    let addr = serve_with(&node, &dir, watch::channel(Manifest::default()).1).await;
    assert_eq!(get(addr, &format!("/v1/archive/{MANIFEST}")).await, (200, Vec::new()));
    assert_eq!(get(addr, "/v1/archive/00000000000000000000.jsonl").await.0, 404);
}

#[tokio::test]
async fn the_stream_refuses_after_and_from_together_or_a_malformed_offset() {
    let dir = scratch_dir("stream-params");
    let node = TestSequencer::start(&dir, 16);
    let addr = serve(&node, &dir).await;
    for query in ["after=1:0&from=0", "after=1", "from=x", "since=0"] {
        let err = connect_async(format!("ws://{addr}/v1/stream?{query}")).await.err().unwrap();
        let tokio_tungstenite::tungstenite::Error::Http(response) = err else { panic!("{query}: {err}") };
        assert_eq!(response.status(), 400, "{query}");
    }
}

#[test]
fn retry_after_is_the_wait_in_whole_seconds_rounded_up() {
    for (wait, header) in [(1500, "2"), (3000, "3"), (1, "1"), (0, "1")] {
        let response = ApiError::RateLimited(Duration::from_millis(wait)).into_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[RETRY_AFTER], header, "a wait of {wait}ms");
    }
}

// ── Helpers ──

/// Serves the API for `node`, with `dir` as its log directory, on a free local port.
async fn serve(node: &TestSequencer, dir: &Path) -> SocketAddr {
    serve_with(node, dir, node.manifest.clone()).await
}

/// Serves the API for `node`, with `dir` as its log directory and `manifest` as the writer's.
async fn serve_with(node: &TestSequencer, dir: &Path, manifest: watch::Receiver<Manifest>) -> SocketAddr {
    let attestation = Arc::new(attest_key(&MockAttestor, &EnclaveKey::generate(), &[0; 32]).unwrap());
    let app = router(ApiState {
        tx: node.tx.clone(),
        stream: node.stream.clone(),
        attestation,
        auth: ClientAuth::Unsigned,
        dev_deposits: true,
        clock: now_ms,
        log_dir: dir.to_owned(),
        manifest,
        backfills: Arc::new(Semaphore::new(MAX_BACKFILLS)),
        limits: Arc::new(ApiLimits::new(&dev_config().api)),
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await });
    addr
}

async fn connect(addr: SocketAddr, query: &str) -> Ws {
    connect_async(format!("ws://{addr}/v1/stream?{query}")).await.unwrap().0
}

/// The next text message, failing the test if none comes within ten seconds.
async fn next_text(ws: &mut Ws) -> String {
    loop {
        let msg = timeout(Duration::from_secs(10), ws.next()).await.expect("a message in time");
        if let WsMessage::Text(text) = msg.expect("stream open").unwrap() {
            return text.to_string();
        }
    }
}

fn offset(msg: &str) -> Offset {
    let value: serde_json::Value = serde_json::from_str(msg).unwrap();
    value["offset"].as_str().unwrap().parse().unwrap()
}

/// A plain HTTP/1.1 GET: the status and the body.
async fn get(addr: SocketAddr, path: &str) -> (u16, Vec<u8>) {
    let mut conn = TcpStream::connect(addr).await.unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    conn.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    timeout(Duration::from_secs(10), conn.read_to_end(&mut response)).await.expect("a response in time").unwrap();
    let split = response.windows(4).position(|w| w == b"\r\n\r\n").expect("headers end");
    let head = String::from_utf8_lossy(&response[..split]).to_lowercase();
    assert!(!head.contains("transfer-encoding: chunked"), "{head}");
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    (status, response[split + 4..].to_vec())
}
