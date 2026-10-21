use engine_core::price::{MAX_QUOTE_LEAD_MS, SourceQuote};

use super::*;
use crate::config::VenueSymbol;
use crate::feeds::kraken;
use crate::feeds::tests::fake::{WAIT, capture_alerts, coinbase_levels, fast, kraken_book, next_alert};
use crate::feeds::venue::quotes::counts;
use crate::feeds::venue::setup::next_backoff;
use crate::throttle::REALERT_AFTER;

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
fn the_newest_quote_the_host_clock_has_reached_is_taken_and_those_ahead_wait() {
    const NOW: Millis = 1_792_489_875_714;
    let at = |ts: Millis| SourceQuote { source: "kraken".into(), mid: Fixed::from_int(100), ts };
    let mut pending = Pending::default();
    // Received over one tick from a venue whose clock is a little ahead of the host's.
    for ts in [NOW - 300, NOW - 100, NOW + 40, NOW + 400] {
        assert_eq!(pending.add(at(ts), NOW), Ok(()));
    }
    assert_eq!(pending.take(NOW), Some(at(NOW - 100)));
    assert_eq!(pending.take(NOW), None);
    // Each quote ahead goes once the host clock reaches it.
    assert_eq!(pending.take(NOW + 40), Some(at(NOW + 40)));
    assert_eq!(pending.take(NOW + 1_000), Some(at(NOW + 400)));
    assert_eq!(pending.take(NOW + 1_000), None);
    // A later quote dated before those still ahead, as after the venue's clock steps back,
    // supersedes them: they never go.
    pending.add(at(NOW + 800), NOW).unwrap();
    pending.add(at(NOW + 500), NOW).unwrap();
    pending.add(at(NOW + 100), NOW + 100).unwrap();
    assert_eq!(pending.take(NOW + 200), Some(at(NOW + 100)));
    assert_eq!(pending.take(NOW + 1_000), None);
    // Of a quote that waited and one received after its time, the newer goes; on a tie, the later.
    pending.add(at(NOW + 50), NOW).unwrap();
    pending.add(SourceQuote { mid: Fixed::from_int(101), ..at(NOW + 50) }, NOW + 60).unwrap();
    assert_eq!(pending.take(NOW + 60).map(|quote| quote.mid), Some(Fixed::from_int(101)));
    pending.add(at(NOW + 70), NOW).unwrap();
    pending.add(at(NOW + 80), NOW + 90).unwrap();
    assert_eq!(pending.take(NOW + 90), Some(at(NOW + 80)));
}

#[test]
fn the_engine_holds_what_it_would_if_every_quote_went_once_its_time_came() {
    // The engine stores a quote in place of the one it holds unless that one is newer and its time
    // already reached.
    let stores = |held: &Option<SourceQuote>, quote: &SourceQuote, now: Millis| {
        held.as_ref().is_none_or(|held| quote.ts >= held.ts || held.ts > now)
    };
    let quote = |id: i64, ts: Millis| SourceQuote { source: "kraken".into(), mid: Fixed::from_int(id), ts };
    // Every way three quotes can arrive at 0, 10, 20 or 30 ms dated 0 to 40 ms, with the engine
    // first holding nothing, a reached quote, or one still ahead, and a tick every 10 ms from 5 ms.
    // The reference sends, at each tick, every quote whose time has come, in arrival order, and
    // withdraws a quote still ahead of the host clock when a later one is dated before it, as when
    // the venue's clock steps back. After every tick the engine holds the same quote either way.
    let times: Vec<(Millis, Millis)> = (0..4).flat_map(|at| (0..5).map(move |ts| (at * 10, ts * 10))).collect();
    let grid = times.as_slice();
    for start in [None, Some(quote(0, -10)), Some(quote(0, 50))] {
        for (first, second, third) in
            grid.iter().flat_map(move |a| grid.iter().flat_map(move |b| grid.iter().map(move |c| (a, b, c))))
        {
            let mut arrivals =
                [(first.0, quote(1, first.1)), (second.0, quote(2, second.1)), (third.0, quote(3, third.1))];
            // Stable, so quotes arriving together keep their order.
            arrivals.sort_by_key(|(at, _)| *at);
            let case = format!("{start:?} then {arrivals:?}");
            let (mut reference, mut held, mut pending) = (start.clone(), start.clone(), Pending::default());
            let mut waiting: Vec<SourceQuote> = Vec::new();
            let mut arrivals = arrivals.into_iter().peekable();
            for tick in (5..=55).step_by(10) {
                while let Some((at, arrived)) = arrivals.next_if(|(at, _)| *at < tick) {
                    waiting.retain(|quote| quote.ts <= at || quote.ts <= arrived.ts);
                    waiting.push(arrived.clone());
                    pending.add(arrived, at).unwrap();
                }
                let (due, ahead): (Vec<_>, Vec<_>) = waiting.into_iter().partition(|quote| quote.ts <= tick);
                waiting = ahead;
                for quote in due {
                    if stores(&reference, &quote, tick) {
                        reference = Some(quote);
                    }
                }
                if let Some(taken) = pending.take(tick)
                    && stores(&held, &taken, tick)
                {
                    held = Some(taken);
                }
                assert_eq!(held, reference, "at {tick}: {case}");
            }
        }
    }
}

#[test]
fn a_kept_book_for_a_symbol_not_subscribed_is_refused_before_it_is_kept() {
    const NOW: Millis = 1_792_489_875_714;
    // Two-sided books and one-sided ones, which have no top for the subscription check to catch.
    let one_sided_coinbase = r#"{"channel":"l2_data","timestamp":"2026-10-20T09:51:15.9Z","sequence_num":1,"events":[{"type":"snapshot","product_id":"SOL-USD","updates":[{"side":"bid","event_time":"2026-10-20T09:51:15.714274Z","price_level":"100","new_quantity":"1"}]}]}"#;
    let checksum = kraken::checksum([("100", "1.5")].into_iter());
    let one_sided_kraken = format!(
        r#"{{"channel":"book","type":"snapshot","data":[{{"symbol":"SOL/USD","bids":[{{"price":100,"qty":1.5}}],"asks":[],"checksum":{checksum},"timestamp":"2026-10-20T12:34:44.332552Z"}}]}}"#
    );
    let cases = [
        (Venue::Coinbase, ["BTC-USD", "ETH-USD"], coinbase_levels(1, true, "SOL-USD", "100", "101", NOW)),
        (Venue::Coinbase, ["BTC-USD", "ETH-USD"], one_sided_coinbase.to_owned()),
        (Venue::Kraken, ["BTC/USD", "ETH/USD"], kraken_book("SOL/USD", "100", "101", NOW)),
        (Venue::Kraken, ["BTC/USD", "ETH/USD"], one_sided_kraken),
    ];
    for (venue, symbols, message) in cases {
        let refused = Adapter::new(venue, &symbols).read(&message, NOW);
        assert!(
            matches!(&refused, Err(Refusal::Connection(reason)) if reason.contains("SOL") && reason.contains("not subscribed")),
            "{}: {refused:?}",
            venue.name()
        );
    }
}

#[test]
fn a_quote_further_ahead_than_the_engine_takes_is_dropped() {
    const NOW: Millis = 1_792_489_875_714;
    let at = |ts: Millis| SourceQuote { source: "kraken".into(), mid: Fixed::from_int(100), ts };
    let mut pending = Pending::default();
    assert_eq!(pending.add(at(NOW + MAX_QUOTE_LEAD_MS), NOW), Ok(()));
    assert_eq!(pending.add(at(NOW + MAX_QUOTE_LEAD_MS + 1), NOW), Err(MAX_QUOTE_LEAD_MS + 1));
    assert_eq!(pending.take(NOW + 60_000), Some(at(NOW + MAX_QUOTE_LEAD_MS)));
}

#[test]
fn a_quote_counts_within_the_tighter_of_the_feed_and_market_bounds() {
    const AT: Millis = 1_792_489_875_714;
    assert!(counts(AT - 1_000, AT, 1_000, 1_000));
    assert!(!counts(AT - 1_001, AT, 1_000, 1_000));
    assert!(!counts(AT + 1, AT, 1_000, 1_000));
    // The market's rules are tighter than the feed's bound, or looser.
    assert!(counts(AT - 500, AT, 1_000, 500));
    assert!(!counts(AT - 501, AT, 1_000, 500));
    assert!(!counts(AT - 1_001, AT, 1_000, 5_000));
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
fn backoff_doubles_up_to_its_cap() {
    let max = Duration::from_secs(30);
    assert_eq!(next_backoff(Duration::from_millis(500), max), Duration::from_secs(1));
    assert_eq!(next_backoff(Duration::from_secs(20), max), max);
    assert_eq!(next_backoff(max, max), max);
}

#[test]
fn a_connection_that_quoted_retries_at_once_and_one_that_did_not_backs_off() {
    let timing =
        Timing { reconnect_min: Duration::from_millis(500), reconnect_max: Duration::from_secs(30), ..fast(WAIT) };
    let grown = Duration::from_secs(8);
    // A routine drop resets the backoff, whatever it had grown to.
    assert_eq!(next_attempt(grown, true, &timing), (timing.reconnect_min, Duration::from_secs(1)));
    // A failed attempt waits the current backoff and doubles it for the next.
    assert_eq!(next_attempt(grown, false, &timing), (grown, Duration::from_secs(16)));
}

// ── Helpers ──

/// A Kraken feed for `symbols` that is never run, for calling its alert paths directly.
fn kraken_feed(symbols: &[(MarketId, &str)], timeout: Duration) -> VenueFeed {
    let symbols = symbols.iter().map(|(market, symbol)| VenueSymbol { market: *market, symbol: (*symbol).into() });
    let conn =
        VenueConnection { venue: Venue::Kraken, url: "wss://ws.kraken.com/v2".into(), symbols: symbols.collect() };
    VenueFeed::new(conn, fast(timeout), tls_config().unwrap(), mpsc::channel(1).0)
}
