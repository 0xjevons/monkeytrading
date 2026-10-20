use super::*;
use crate::config::VenueSymbol;
use crate::feeds::tests::fake::{WAIT, capture_alerts, fast, next_alert};

#[test]
fn a_throttle_lets_one_alert_through_a_window_and_counts_the_rest() {
    let mut throttle = Throttle::default();
    let start = Instant::now();
    assert_eq!(throttle.ready(start), Some(0));
    assert_eq!(throttle.ready(start + Duration::from_secs(1)), None);
    assert_eq!(throttle.ready(start + REALERT_AFTER - Duration::from_millis(1)), None);
    assert_eq!(throttle.ready(start + REALERT_AFTER), Some(2));
    assert_eq!(throttle.ready(start + REALERT_AFTER), None);
}

#[tokio::test(start_paused = true)]
async fn refusals_alert_again_after_the_window_with_the_count_between() {
    let (_guard, mut alerts) = capture_alerts();
    let mut feed = kraken_feed(&[(1, "BTC/USD")], WAIT);

    ["first", "second", "third"].into_iter().for_each(|reason| feed.refuse(reason, ""));
    tokio::time::advance(REALERT_AFTER).await;
    feed.refuse("fourth", "");

    let first = next_alert(&mut alerts).await;
    assert_eq!((first["reason"].as_str(), first["suppressed"].as_str()), ("first", "0"));
    let again = next_alert(&mut alerts).await;
    assert_eq!((again["reason"].as_str(), again["suppressed"].as_str()), ("fourth", "2"));
    assert!(alerts.try_recv().is_err());
}

#[tokio::test(start_paused = true)]
async fn clock_alerts_are_held_to_one_a_window() {
    let (_guard, mut alerts) = capture_alerts();
    let mut feed = kraken_feed(&[(1, "BTC/USD")], WAIT);

    (0..5).for_each(|_| feed.clock_ahead(1, 60_000));
    tokio::time::advance(REALERT_AFTER).await;
    feed.clock_ahead(1, 60_000);

    let first = next_alert(&mut alerts).await;
    assert_eq!((first["alert"].as_str(), first["suppressed"].as_str()), ("venue_clock_ahead", "0"));
    let again = next_alert(&mut alerts).await;
    assert_eq!((again["alert"].as_str(), again["suppressed"].as_str()), ("venue_clock_ahead", "4"));
    assert!(alerts.try_recv().is_err());
}

#[test]
fn the_newest_quote_the_engine_can_use_is_sequenced() {
    const NOW: Millis = 1_792_489_875_714;
    let at = |ts: Millis| SourceQuote { source: "kraken".into(), mid: Fixed::from_int(100), ts };
    let received = |times: &[Millis]| times.iter().map(|ts| at(*ts)).collect::<VecDeque<_>>();
    // A host clock a little behind the venue: the newest already reached is used, not the newest.
    assert_eq!(select(&received(&[NOW - 300, NOW - 100, NOW + 40, NOW - 200]), NOW), Some(at(NOW - 100)));
    assert_eq!(select(&received(&[NOW - 100, NOW]), NOW), Some(at(NOW)));
    // Every quote ahead, as after the host clock is set back: the newest goes, and the engine
    // skips it rather than keep pricing from older quotes.
    assert_eq!(select(&received(&[NOW + 50, NOW + 80, NOW + 60]), NOW), Some(at(NOW + 80)));
    assert_eq!(select(&VecDeque::new(), NOW), None);
}

#[tokio::test(start_paused = true)]
async fn a_recovery_gives_every_symbol_a_full_silence_window() {
    let (_guard, mut alerts) = capture_alerts();
    let window = Duration::from_millis(300);
    let mut feed = kraken_feed(&[(1, "BTC/USD"), (2, "ETH/USD")], window);
    // Down for longer than the window, so both symbols' counts are past it.
    feed.down = true;
    tokio::time::advance(window * 2).await;

    // BTC quotes again; ETH's count restarts with the recovery, not from before the outage.
    feed.delivered(1);
    tokio::time::advance(window - Duration::from_millis(1)).await;
    feed.delivered(1);
    assert!(!feed.check_silent());
    assert!(alerts.try_recv().is_err());
    tokio::time::advance(Duration::from_millis(1)).await;
    assert!(feed.check_silent());
    let alert = next_alert(&mut alerts).await;
    assert_eq!((alert["alert"].as_str(), alert["symbol"].as_str()), ("venue_symbol_silent", "ETH/USD"));
}

#[test]
fn a_full_buffer_drops_its_oldest_quote() {
    let at = |ts: Millis| SourceQuote { source: "kraken".into(), mid: Fixed::from_int(100), ts };
    let mut quotes = VecDeque::new();
    (0..=MAX_PENDING as Millis).for_each(|ts| push_pending(&mut quotes, at(ts)));
    assert_eq!(quotes.len(), MAX_PENDING);
    assert_eq!(quotes.front(), Some(&at(1)));
    assert_eq!(quotes.back(), Some(&at(MAX_PENDING as Millis)));
}

// ── Helpers ──

/// A Kraken feed for `symbols` that is never run, for calling its alert paths directly.
fn kraken_feed(symbols: &[(MarketId, &str)], timeout: Duration) -> VenueFeed {
    let symbols = symbols.iter().map(|(market, symbol)| VenueSymbol { market: *market, symbol: (*symbol).into() });
    let conn =
        VenueConnection { venue: Venue::Kraken, url: "wss://ws.kraken.com/v2".into(), symbols: symbols.collect() };
    VenueFeed::new(conn, fast(timeout), tls_config().unwrap(), mpsc::channel(1).0)
}
