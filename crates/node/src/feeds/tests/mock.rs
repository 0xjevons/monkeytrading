//! The mock feed: each mock market quotes until its `stop_after`.

use std::time::Duration;

use engine_core::Fixed;
use engine_core::command::Command;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::config::{FeedProvider, FeedsSection, MockMid};
use crate::feeds::run_mock;
use crate::sequencer::Request;

#[tokio::test(start_paused = true)]
async fn a_mock_market_stops_quoting_at_its_stop_after_and_the_others_carry_on() {
    let mock_mid = |market, stop_after| MockMid { market, mid: Fixed::from_int(100), stop_after };
    let cfg = FeedsSection {
        provider: FeedProvider::Mock,
        interval: Duration::from_millis(100),
        mock_mids: vec![mock_mid(1, Some(Duration::from_millis(250))), mock_mid(2, None)],
        venues: None,
    };
    let (tx, mut rx) = mpsc::channel(64);
    let started = Instant::now();
    tokio::spawn(async move { run_mock(&cfg, tx).await });
    let mut quoted = Vec::new();
    while started.elapsed() < Duration::from_secs(1) {
        let Some(Request::Submit { cmd: Command::Prices { market, .. }, .. }) = rx.recv().await else {
            panic!("expected prices");
        };
        quoted.push((started.elapsed().as_millis(), market));
    }
    let last = |id| quoted.iter().filter(|(_, market)| *market == id).map(|(at, _)| *at).max();
    assert_eq!(last(1), Some(200));
    assert_eq!(last(2), Some(1_000));
}
