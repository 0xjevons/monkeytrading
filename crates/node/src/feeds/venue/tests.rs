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
    let mut feed = kraken_feed();

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
    let mut feed = kraken_feed();

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

// ── Helpers ──

/// A Kraken feed for BTC/USD that is never run, for calling its alert paths directly.
fn kraken_feed() -> VenueFeed {
    let symbols = vec![VenueSymbol { market: 1, symbol: "BTC/USD".into() }];
    let conn = VenueConnection { venue: Venue::Kraken, url: "wss://ws.kraken.com/v2".into(), symbols };
    VenueFeed::new(conn, fast(WAIT), tls_config().unwrap(), mpsc::channel(1).0)
}
