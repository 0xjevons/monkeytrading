use super::*;

// Recorded from wss://ws.kraken.com/v2: an ETH/USD snapshot and the four updates that followed it.
const SNAPSHOT: &str = r#"{"channel":"book","type":"snapshot","data":[{"symbol":"ETH/USD","bids":[{"price":2387.34,"qty":1.11000000},{"price":2387.16,"qty":0.03207130},{"price":2387.15,"qty":2.09886512},{"price":2387.14,"qty":0.01300000},{"price":2387.13,"qty":2.09456314},{"price":2387.06,"qty":0.47001888},{"price":2386.97,"qty":0.44710000},{"price":2386.96,"qty":0.20000000},{"price":2386.89,"qty":0.01600000},{"price":2386.87,"qty":1.54940553}],"asks":[{"price":2387.35,"qty":13.07562979},{"price":2387.42,"qty":1.89091043},{"price":2387.43,"qty":2.09430224},{"price":2387.52,"qty":4.24040608},{"price":2387.53,"qty":16.31597844},{"price":2387.54,"qty":31.41396556},{"price":2387.57,"qty":0.35563916},{"price":2387.59,"qty":0.44710000},{"price":2387.64,"qty":0.13118718},{"price":2387.69,"qty":1.35850000}],"checksum":602132210,"timestamp":"2026-10-20T12:34:44.332552Z"}]}"#;
const UPDATES: [&str; 4] = [
    r#"{"channel":"book","type":"update","data":[{"symbol":"ETH/USD","bids":[],"asks":[{"price":2387.35,"qty":3.07562993}],"checksum":3352854302,"timestamp":"2026-10-20T12:34:44.338822Z"}]}"#,
    r#"{"channel":"book","type":"update","data":[{"symbol":"ETH/USD","bids":[],"asks":[{"price":2387.35,"qty":2.62852993}],"checksum":3241282715,"timestamp":"2026-10-20T12:34:44.389205Z"}]}"#,
    r#"{"channel":"book","type":"update","data":[{"symbol":"ETH/USD","bids":[{"price":2387.34,"qty":1.55710000}],"asks":[],"checksum":589154695,"timestamp":"2026-10-20T12:34:44.390458Z"}]}"#,
    // Removes a bid inside the top ten and adds one at the edge.
    r#"{"channel":"book","type":"update","data":[{"symbol":"ETH/USD","bids":[{"price":2387.15,"qty":0.00000000},{"price":2386.82,"qty":6.37118262}],"asks":[],"checksum":3655021122,"timestamp":"2026-10-20T12:34:44.401224Z"}]}"#,
];
const HEARTBEAT: &str = r#"{"channel":"heartbeat"}"#;
const STATUS: &str = r#"{"channel":"status","type":"update","data":[{"version":"2.0.10","system":"online","api_version":"v2","connection_id":5169314452737955867,"upcoming_maintenance":[],"emergency":[]}]}"#;
const SUBSCRIBED: &str = r#"{"method":"subscribe","result":{"channel":"book","depth":10,"snapshot":true,"symbol":"ETH/USD"},"success":true,"time_in":"2026-10-20T12:34:28.967739Z","time_out":"2026-10-20T12:34:28.967768Z"}"#;
const REJECTED: &str = r#"{"error":"Currency pair not supported BTC/USX","method":"subscribe","success":false,"symbol":"BTC/USX","time_in":"2026-10-20T09:43:52.433866Z","time_out":"2026-10-20T09:43:52.433915Z"}"#;

#[test]
fn a_snapshot_gives_its_verified_top_at_the_venue_time() {
    // 2026-10-20T12:34:44.332552Z, truncated to the millisecond.
    assert_eq!(Books::default().read(SNAPSHOT), Ok(top("2387.34", "2387.35", 1_792_499_684_332)));
}

#[test]
fn every_verified_update_publishes_the_top_at_its_time() {
    let mut books = Books::default();
    books.read(SNAPSHOT).unwrap();
    // The first three change sizes only: the top holds but is published at each update's time.
    let times = [1_792_499_684_338, 1_792_499_684_389, 1_792_499_684_390, 1_792_499_684_401];
    for (update, ts) in UPDATES.iter().zip(times) {
        assert_eq!(books.read(update), Ok(top("2387.34", "2387.35", ts)), "{update}");
    }
}

#[test]
fn a_corrupted_checksum_resyncs_without_a_quote() {
    let mut books = Books::default();
    books.read(SNAPSHOT).unwrap();
    let corrupted = UPDATES[0].replace(r#""checksum":3352854302"#, r#""checksum":3352854303"#);
    assert!(matches!(books.read(&corrupted), Err(Refusal::Connection(reason)) if reason.contains("checksum mismatch")));
}

#[test]
fn a_corrupted_level_resyncs_without_a_quote() {
    let mut books = Books::default();
    books.read(SNAPSHOT).unwrap();
    // The price is valid and in sequence, but not the venue's: only the checksum shows it.
    let corrupted = UPDATES[0].replace(r#""qty":3.07562993"#, r#""qty":3.07562994"#);
    assert!(matches!(books.read(&corrupted), Err(Refusal::Connection(reason)) if reason.contains("checksum mismatch")));
}

#[test]
fn a_missed_update_is_caught_by_the_next_checksum() {
    let mut books = Books::default();
    books.read(SNAPSHOT).unwrap();
    books.read(UPDATES[0]).unwrap();
    books.read(UPDATES[1]).unwrap();
    // UPDATES[2] never arrives; the book misses its new bid size.
    assert!(matches!(books.read(UPDATES[3]), Err(Refusal::Connection(reason)) if reason.contains("checksum mismatch")));
}

#[test]
fn malformed_books_resync() {
    let cases = [
        // Exponent notation is not a plain decimal; reading it would mean going through a float.
        UPDATES[0].replace(r#""price":2387.35"#, r#""price":2.38735e3"#),
        UPDATES[0].replace(r#""price":2387.35"#, r#""price":"2387.35""#),
        UPDATES[0].replace(r#""qty":3.07562993"#, r#""qty":-3.07562993"#),
        UPDATES[0].replace(r#""price":2387.35"#, r#""price":0"#),
        UPDATES[0].replace(r#""checksum":3352854302,"#, ""),
        UPDATES[0].replace("2026-10-20T12:34:44.338822Z", "later"),
        UPDATES[0].replace(r#""type":"update""#, r#""type":"delta""#),
        UPDATES[0].replace("ETH/USD", "SOL/USD"),
    ];
    for case in cases {
        let mut books = Books::default();
        books.read(SNAPSHOT).unwrap();
        assert!(matches!(books.read(&case), Err(Refusal::Connection(_))), "applied {case}");
    }
}

#[test]
fn heartbeats_status_and_acknowledgements_carry_no_price() {
    let mut books = Books::default();
    assert_eq!(books.read(HEARTBEAT), Ok(Frame::Control));
    assert_eq!(books.read(STATUS), Ok(Frame::Control));
    assert_eq!(books.read(SUBSCRIBED), Ok(Frame::Control));
}

#[test]
fn a_failed_subscription_or_unknown_message_is_refused() {
    let mut books = Books::default();
    assert!(
        matches!(books.read(REJECTED), Err(Refusal::Message(reason)) if reason.contains("Currency pair not supported"))
    );
    assert!(matches!(books.read(r#"{"channel":"trade"}"#), Err(Refusal::Message(_))));
    assert!(matches!(books.read("{"), Err(Refusal::Message(_))));
}

#[test]
fn the_subscription_asks_for_ten_levels_of_every_symbol() {
    let sent: serde_json::Value = serde_json::from_str(&subscriptions(&["BTC/USD", "ETH/USD"])[0]).unwrap();
    let params = json!({ "channel": "book", "symbol": ["BTC/USD", "ETH/USD"], "depth": 10 });
    assert_eq!(sent, json!({ "method": "subscribe", "params": params }));
}

// ── Helpers ──

/// An ETH/USD top of book.
fn top(bid: &str, ask: &str, ts: i64) -> Frame {
    Frame::Books(vec![Book { symbol: "ETH/USD".into(), bid: bid.parse().unwrap(), ask: ask.parse().unwrap(), ts }])
}
