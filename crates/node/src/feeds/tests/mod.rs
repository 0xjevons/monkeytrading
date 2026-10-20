//! Venue feed tasks against loopback venues: what they sequence, what they refuse, and how they
//! reconnect.

mod fake;
mod scenario;

use std::collections::BTreeMap;
use std::time::Duration;

use tokio::time::{sleep, timeout};

use crate::clock::now_ms;
use crate::config::Venue;
use crate::feeds::tests::fake::{
    WAIT, accept, bitstamp_book, capture_alerts, coinbase_levels, fast, kraken_ticker, listen, next_alert, next_prices,
    quote, send, spawn_feed,
};
use crate::feeds::venue::{Book, Timing, next_backoff, to_quote};
use crate::feeds::{bitstamp, kraken};

const MARKETS: [(u32, &str); 2] = [(1, "BTC/USD"), (2, "ETH/USD")];

#[tokio::test]
async fn quotes_are_sequenced_per_market_with_the_venue_timestamp() {
    let (listener, url) = listen().await;
    let (mut rx, _feed) = spawn_feed(Venue::Kraken, &url, &MARKETS, fast(WAIT));
    let (mut ws, subscription) = accept(&listener).await;
    assert_eq!(subscription, kraken::subscriptions(&["BTC/USD", "ETH/USD"])[0]);

    send(
        &mut ws,
        &[kraken_ticker("BTC/USD", "81250.5", "81251.5", 1_000), kraken_ticker("ETH/USD", "2377", "2377.1", 2_000)],
    )
    .await;
    let mut sequenced = BTreeMap::from([next_prices(&mut rx).await, next_prices(&mut rx).await]);
    assert_eq!(sequenced.remove(&1), Some(vec![quote(Venue::Kraken, "81251", 1_000)]));
    assert_eq!(sequenced.remove(&2), Some(vec![quote(Venue::Kraken, "2377.05", 2_000)]));
}

#[tokio::test]
async fn only_the_latest_quote_per_market_is_sequenced_each_interval() {
    let (listener, url) = listen().await;
    let timing = Timing { interval: Duration::from_secs(1), ..fast(WAIT) };
    let (mut rx, _feed) = spawn_feed(Venue::Kraken, &url, &MARKETS, timing);
    let (mut ws, _) = accept(&listener).await;

    // All three arrive within one interval, so only the last is sequenced.
    let tickers = [1_000, 1_100, 1_200].map(|ts| kraken_ticker("BTC/USD", "100", "101", ts));
    send(&mut ws, &tickers).await;
    assert_eq!(next_prices(&mut rx).await, (1, vec![quote(Venue::Kraken, "100.5", 1_200)]));
}

#[tokio::test]
async fn malformed_messages_and_books_are_refused_with_an_alert() {
    let (_guard, mut alerts) = capture_alerts();
    let (listener, url) = listen().await;
    // Every message lands within one interval, so a refused book accepted after the valid one
    // would replace it as the quote sequenced.
    let timing = Timing { interval: Duration::from_secs(1), ..fast(WAIT) };
    let (mut rx, _feed) = spawn_feed(Venue::Kraken, &url, &MARKETS, timing);
    let (mut ws, _) = accept(&listener).await;

    let messages = [
        kraken_ticker("BTC/USD", "100", "101", 1_000),
        "not json".to_owned(),
        kraken_ticker("BTC/USD", "101", "100", 2_000),
        kraken_ticker("BTC/USD", "0", "100", 2_000),
        kraken_ticker("BTC/USD", "100", "1000000001", 2_000),
        kraken_ticker("BTC/USD", "100", "102", now_ms() + 60_000),
        kraken_ticker("SOL/USD", "100", "101", 2_000),
    ];
    send(&mut ws, &messages).await;

    // Nothing refused reaches the sequencer, and the first refusal alerts.
    assert_eq!(next_prices(&mut rx).await, (1, vec![quote(Venue::Kraken, "100.5", 1_000)]));
    let alert = next_alert(&mut alerts).await;
    assert_eq!(alert["alert"], "venue_message_refused");
    assert_eq!(alert["venue"], "kraken");
    // The other refusals within the re-alert window are counted, not alerted.
    assert!(alerts.try_recv().is_err());
}

#[tokio::test]
async fn a_dropped_connection_reconnects_and_quotes_resume_without_an_alert() {
    let (_guard, mut alerts) = capture_alerts();
    let (listener, url) = listen().await;
    let (mut rx, _feed) = spawn_feed(Venue::Kraken, &url, &MARKETS, fast(WAIT));

    let (mut first, _) = accept(&listener).await;
    let ts = now_ms();
    send(&mut first, &[kraken_ticker("BTC/USD", "100", "101", ts)]).await;
    assert_eq!(next_prices(&mut rx).await, (1, vec![quote(Venue::Kraken, "100.5", ts)]));
    drop(first);

    let (mut second, subscription) = accept(&listener).await;
    assert_eq!(subscription, kraken::subscriptions(&["BTC/USD", "ETH/USD"])[0]);
    let ts = now_ms();
    send(&mut second, &[kraken_ticker("BTC/USD", "100", "102", ts)]).await;
    assert_eq!(next_prices(&mut rx).await, (1, vec![quote(Venue::Kraken, "101", ts)]));
    // A connection that had sequenced fresh quotes is a routine drop, not an outage.
    assert!(alerts.try_recv().is_err());
}

#[tokio::test]
async fn a_connection_that_quoted_reconnects_without_growing_the_backoff() {
    let (listener, url) = listen().await;
    let timing = Timing { reconnect_max: Duration::from_secs(60), ..fast(WAIT) };
    let (mut rx, _feed) = spawn_feed(Venue::Kraken, &url, &MARKETS, timing);
    // Nine reconnects at 10ms each fit easily; doubling from 10ms would take over five seconds.
    let reconnects = async {
        for _ in 0..10 {
            let (mut ws, _) = accept(&listener).await;
            let ts = now_ms();
            send(&mut ws, &[kraken_ticker("BTC/USD", "100", "101", ts)]).await;
            assert_eq!(next_prices(&mut rx).await, (1, vec![quote(Venue::Kraken, "100.5", ts)]));
        }
    };
    timeout(Duration::from_secs(2), reconnects).await.expect("the backoff grew between quoting connections");
}

#[tokio::test]
async fn a_handshake_that_never_completes_times_out_and_alerts() {
    let (_guard, mut alerts) = capture_alerts();
    let (listener, url) = listen().await;
    let (_rx, _feed) = spawn_feed(Venue::Kraken, &url, &MARKETS, fast(Duration::from_millis(200)));

    // The first attempt reaches TCP, which is held open without answering the handshake.
    let (_held, _) = timeout(WAIT, listener.accept()).await.expect("no attempt in time").expect("accept");
    let alert = next_alert(&mut alerts).await;
    assert_eq!((alert["alert"].as_str(), alert["venue"].as_str()), ("venue_feed_down", "kraken"));
    timeout(WAIT, listener.accept()).await.expect("no retry after the timeout").expect("accept");
}

#[tokio::test]
async fn an_unreachable_venue_alerts_once_and_keeps_retrying() {
    let (_guard, mut alerts) = capture_alerts();
    let (listener, url) = listen().await;
    let (_rx, feed) = spawn_feed(Venue::Kraken, &url, &MARKETS, fast(WAIT));

    // Each attempt reaches TCP and is cut before the WebSocket handshake.
    for _ in 0..3 {
        let (tcp, _) = timeout(WAIT, listener.accept()).await.expect("no attempt in time").expect("accept");
        drop(tcp);
    }
    let alert = next_alert(&mut alerts).await;
    assert_eq!((alert["alert"].as_str(), alert["venue"].as_str()), ("venue_feed_down", "kraken"));
    assert!(alerts.try_recv().is_err(), "the outage alerted more than once");
    assert!(!feed.is_finished());
}

#[tokio::test]
async fn a_silent_connection_is_dropped_and_retried() {
    let (_guard, mut alerts) = capture_alerts();
    let (listener, url) = listen().await;
    let (_rx, _feed) = spawn_feed(Venue::Kraken, &url, &MARKETS, fast(Duration::from_millis(200)));

    let (_silent, _) = accept(&listener).await;
    // The feed gives up on the silent connection and subscribes afresh.
    let (_next, subscription) = accept(&listener).await;
    assert_eq!(subscription, kraken::subscriptions(&["BTC/USD", "ETH/USD"])[0]);
    let outage = async { while next_alert(&mut alerts).await["alert"] != "venue_feed_down" {} };
    timeout(WAIT, outage).await.expect("no outage alert");
}

#[tokio::test]
async fn a_symbol_the_venue_never_quotes_is_alerted() {
    let (_guard, mut alerts) = capture_alerts();
    let (listener, url) = listen().await;
    let (mut rx, _feed) = spawn_feed(Venue::Kraken, &url, &MARKETS, fast(Duration::from_millis(300)));
    let (mut ws, _) = accept(&listener).await;

    // BTC keeps quoting fresh while ETH never quotes.
    let streaming = tokio::spawn(async move {
        for _ in 0..1_000 {
            send(&mut ws, &[kraken_ticker("BTC/USD", "100", "101", now_ms())]).await;
            sleep(Duration::from_millis(20)).await;
        }
    });
    next_prices(&mut rx).await;
    let alert = next_alert(&mut alerts).await;
    assert_eq!((alert["alert"].as_str(), alert["symbol"].as_str()), ("venue_symbol_silent", "ETH/USD"));
    streaming.abort();
}

#[tokio::test]
async fn stale_quotes_do_not_count_as_the_venue_quoting() {
    let (_guard, mut alerts) = capture_alerts();
    let (listener, url) = listen().await;
    let (mut rx, _feed) = spawn_feed(Venue::Kraken, &url, &[(1, "BTC/USD")], fast(Duration::from_millis(300)));
    let (mut ws, _) = accept(&listener).await;

    // A stuck venue repeats a book a minute old; the engine still gets it and judges its age.
    let ts = now_ms() - 60_000;
    let streaming = tokio::spawn(async move {
        for _ in 0..1_000 {
            send(&mut ws, &[kraken_ticker("BTC/USD", "100", "101", ts)]).await;
            sleep(Duration::from_millis(20)).await;
        }
    });
    assert_eq!(next_prices(&mut rx).await, (1, vec![quote(Venue::Kraken, "100.5", ts)]));
    let alert = next_alert(&mut alerts).await;
    assert_eq!((alert["alert"].as_str(), alert["symbol"].as_str()), ("venue_symbol_silent", "BTC/USD"));
    streaming.abort();
}

#[tokio::test]
async fn a_book_that_falls_out_of_sync_resyncs_with_an_alert() {
    let (_guard, mut alerts) = capture_alerts();
    let (listener, url) = listen().await;
    let (mut rx, _feed) = spawn_feed(Venue::Coinbase, &url, &[(1, "BTC-USD")], fast(WAIT));

    let (mut first, _) = accept(&listener).await;
    send(&mut first, &[coinbase_levels(0, true, "BTC-USD", "100", "101", 1_000)]).await;
    assert_eq!(next_prices(&mut rx).await, (1, vec![quote(Venue::Coinbase, "100.5", 1_000)]));
    // Sequence 1 never arrives.
    send(&mut first, &[coinbase_levels(2, false, "BTC-USD", "100.5", "101", 2_000)]).await;

    let alert = next_alert(&mut alerts).await;
    assert_eq!((alert["alert"].as_str(), alert["venue"].as_str()), ("venue_message_refused", "coinbase"));
    let (mut second, _) = accept(&listener).await;
    send(&mut second, &[coinbase_levels(0, true, "BTC-USD", "100", "102", 3_000)]).await;
    assert_eq!(next_prices(&mut rx).await, (1, vec![quote(Venue::Coinbase, "101", 3_000)]));
}

#[tokio::test]
async fn a_refused_book_from_a_kept_book_resyncs() {
    let (_guard, mut alerts) = capture_alerts();
    let (listener, url) = listen().await;
    let (mut rx, _feed) = spawn_feed(Venue::Coinbase, &url, &[(1, "BTC-USD")], fast(WAIT));

    let (mut first, _) = accept(&listener).await;
    send(&mut first, &[coinbase_levels(0, true, "BTC-USD", "100", "101", 1_000)]).await;
    assert_eq!(next_prices(&mut rx).await, (1, vec![quote(Venue::Coinbase, "100.5", 1_000)]));
    // In sequence, but dated a minute ahead: its levels must not stay in the kept book.
    send(&mut first, &[coinbase_levels(1, false, "BTC-USD", "100.2", "101", now_ms() + 60_000)]).await;

    let alert = next_alert(&mut alerts).await;
    assert_eq!((alert["alert"].as_str(), alert["venue"].as_str()), ("venue_message_refused", "coinbase"));
    let (mut second, _) = accept(&listener).await;
    send(&mut second, &[coinbase_levels(0, true, "BTC-USD", "100", "102", 3_000)]).await;
    assert_eq!(next_prices(&mut rx).await, (1, vec![quote(Venue::Coinbase, "101", 3_000)]));
}

#[tokio::test]
async fn a_reconnect_request_ends_the_connection() {
    let (listener, url) = listen().await;
    let (mut rx, _feed) = spawn_feed(Venue::Bitstamp, &url, &[(1, "btcusd")], fast(WAIT));

    let (mut first, _) = accept(&listener).await;
    send(&mut first, &[r#"{"event":"bts:request_reconnect","channel":"","data":""}"#.to_owned()]).await;
    let (mut second, subscription) = accept(&listener).await;
    assert_eq!(subscription, bitstamp::subscriptions(&["btcusd"])[0]);
    send(&mut second, &[bitstamp_book("btcusd", "100", "101", 1_000)]).await;
    assert_eq!(next_prices(&mut rx).await, (1, vec![quote(Venue::Bitstamp, "100.5", 1_000)]));
}

#[tokio::test]
async fn the_feed_stops_when_the_sequencer_goes_away() {
    let (listener, url) = listen().await;
    let (rx, feed) = spawn_feed(Venue::Kraken, &url, &MARKETS, fast(WAIT));
    drop(rx);
    let (mut ws, _) = accept(&listener).await;
    send(&mut ws, &[kraken_ticker("BTC/USD", "100", "101", 1_000)]).await;

    let stopped = timeout(WAIT, feed).await.expect("feed kept running").expect("feed panicked");
    let err = format!("{:#}", stopped.unwrap_err());
    assert!(err.contains("sequencer gone"), "{err}");
    // The feed never reconnects once the sequencer is gone.
    assert!(timeout(Duration::from_millis(100), listener.accept()).await.is_err());
}

#[test]
fn books_no_working_venue_could_show_are_refused() {
    const NOW: i64 = 1_792_489_875_714;
    let markets = BTreeMap::from([("BTC/USD".to_owned(), 1)]);
    let book = |symbol: &str, bid: &str, ask: &str, ts| Book {
        symbol: symbol.into(),
        bid: bid.parse().unwrap(),
        ask: ask.parse().unwrap(),
        ts,
    };
    let refused = [
        ("unsubscribed symbol", book("SOL/USD", "100", "101", 1)),
        ("crossed", book("BTC/USD", "101", "100", 1)),
        ("crossed", book("BTC/USD", "100", "100", 1)),
        ("out of range", book("BTC/USD", "0", "100", 1)),
        ("out of range", book("BTC/USD", "100", "1000000000.1", 1)),
        ("before the epoch", book("BTC/USD", "100", "101", -1)),
        ("1001ms ahead of the host clock", book("BTC/USD", "100", "101", NOW + 1_001)),
    ];
    for (expected, book) in refused {
        let err = to_quote(Venue::Kraken, &markets, book, NOW).unwrap_err();
        assert!(err.contains(expected), "expected {expected:?}, got {err}");
    }
    // A time up to a second ahead of the host clock passes, and the mid truncates toward zero at
    // the ninth decimal.
    let accepted = to_quote(Venue::Kraken, &markets, book("BTC/USD", "100", "100.000000001", NOW + 1_000), NOW);
    assert_eq!(accepted, Ok((1, quote(Venue::Kraken, "100", NOW + 1_000))));
}

#[test]
fn backoff_doubles_up_to_its_cap() {
    let max = Duration::from_secs(30);
    assert_eq!(next_backoff(Duration::from_millis(500), max), Duration::from_secs(1));
    assert_eq!(next_backoff(Duration::from_secs(20), max), max);
    assert_eq!(next_backoff(max, max), max);
}
