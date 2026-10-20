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
    let symbols = vec![VenueSymbol { market: 1, symbol: "BTC/USD".into() }];
    let conn = VenueConnection { venue: Venue::Kraken, url: "wss://ws.kraken.com/v2".into(), symbols };
    let mut feed = VenueFeed::new(conn, fast(WAIT), tls_config().unwrap(), mpsc::channel(1).0);

    ["first", "second", "third"].into_iter().for_each(|reason| feed.refuse(reason, ""));
    tokio::time::advance(REALERT_AFTER).await;
    feed.refuse("fourth", "");

    let first = next_alert(&mut alerts).await;
    assert_eq!((first["reason"].as_str(), first["suppressed"].as_str()), ("first", "0"));
    let again = next_alert(&mut alerts).await;
    assert_eq!((again["reason"].as_str(), again["suppressed"].as_str()), ("fourth", "2"));
    assert!(alerts.try_recv().is_err());
}
