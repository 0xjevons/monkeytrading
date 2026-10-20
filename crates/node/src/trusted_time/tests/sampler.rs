//! The trusted time task's rounds: queries, back-off, and the alerts they raise.

use super::*;

/// Long enough for a loopback reply on a busy runner.
const WAIT: Duration = Duration::from_secs(5);

#[tokio::test]
async fn a_query_verifies_the_reply_and_bounds_true_time_when_it_arrives() {
    let clock = Arc::new(TestClock::default());
    let server = FakeServer::start(1, VERSION_DRAFT, 2, clock.clone()).await;
    let reply = query(&server.address, &server.key, clock.as_ref(), WAIT).await.unwrap();
    // The midpoint is whole seconds and the test clock stands still, so the reply spans exactly
    // the radius either side of the second true time falls in.
    let second_ms = clock.true_ms() / 1000 * 1000;
    assert_eq!((reply.earliest_ms, reply.latest_ms), (second_ms - 2_000, second_ms + 2_000));
}

#[tokio::test]
async fn a_query_refuses_a_reply_signed_by_another_key() {
    let clock = Arc::new(TestClock::default());
    let server = FakeServer::start(1, VERSION_1, 2, clock.clone()).await;
    let other = TestServer::new(2, VERSION_1).public_key();
    let err = query(&server.address, &other, clock.as_ref(), WAIT).await.unwrap_err();
    assert!(format!("{err:#}").contains("delegation not signed"), "{err:#}");
}

#[tokio::test]
async fn a_query_to_an_address_without_a_port_fails_to_resolve() {
    let clock = TestClock::default();
    let key = TestServer::new(1, VERSION_1).public_key();
    let err = query("127.0.0.1", &key, &clock, WAIT).await.unwrap_err();
    assert!(format!("{err:#}").contains("resolving 127.0.0.1"), "{err:#}");
}

#[tokio::test]
async fn a_query_falls_back_past_an_address_that_fails_or_stays_silent() {
    let clock = Arc::new(TestClock::default());
    let good = FakeServer::start(1, VERSION_1, 2, clock.clone()).await;
    let silent = FakeServer::start(1, VERSION_1, 2, clock.clone()).await;
    silent.answering.store(false, Ordering::SeqCst);
    let closed: SocketAddr = closed_port().await.parse().unwrap();
    let [good_at, silent_at] = [&good, &silent].map(|s| s.address.parse::<SocketAddr>().unwrap());
    let soon = || TokioInstant::now() + Duration::from_millis(600);
    let reply = query_addresses(&[closed, silent_at, good_at], &good.key, clock.as_ref(), soon()).await;
    assert!(reply.is_ok(), "{reply:?}");

    let err = query_addresses(&[closed, silent_at], &good.key, clock.as_ref(), soon()).await.unwrap_err();
    assert!(format!("{err:#}").contains(&format!("{silent_at}: no reply within")), "{err:#}");
    assert!(format!("{err:#}").contains(&closed.to_string()), "{err:#}");
    assert!(query_addresses(&[], &good.key, clock.as_ref(), soon()).await.is_err());
}

#[tokio::test]
async fn however_many_addresses_stay_silent_after_a_slow_lookup_a_query_ends_by_its_deadline() {
    let clock = Arc::new(TestClock::default());
    let good = FakeServer::start(1, VERSION_1, 2, clock.clone()).await;
    let mut quiet_servers = Vec::new();
    for _ in 0..3 {
        let server = FakeServer::start(1, VERSION_1, 2, clock.clone()).await;
        server.answering.store(false, Ordering::SeqCst);
        quiet_servers.push(server);
    }
    let silent: Vec<SocketAddr> = quiet_servers.iter().map(|s| s.address.parse().unwrap()).collect();
    let good_at: SocketAddr = good.address.parse().unwrap();
    for quiet in 0..=silent.len() {
        let peers: Vec<SocketAddr> = silent[..quiet].iter().copied().chain([good_at]).collect();
        // A third of the budget is gone, as after a slow lookup.
        let deadline = TokioInstant::now() + Duration::from_millis(450);
        tokio::time::sleep(Duration::from_millis(150)).await;
        let reply = query_addresses(&peers, &good.key, clock.as_ref(), deadline).await;
        assert!(reply.is_ok(), "{quiet} silent first: {reply:?}");
        assert!(TokioInstant::now() <= deadline, "{quiet} silent first: answered after the deadline");
    }
    // With every address silent, the query gives up by its deadline.
    let deadline = TokioInstant::now() + Duration::from_millis(300);
    assert!(query_addresses(&silent, &good.key, clock.as_ref(), deadline).await.is_err());
    assert!(TokioInstant::now() <= deadline + Duration::from_millis(50));
}

#[test]
fn a_single_failed_query_neither_alerts_nor_logs_a_recovery() {
    let (logs, _guard) = Logs::capture();
    let key = TestServer::new(1, VERSION_1).public_key();
    let mut server =
        Server::new(RoughtimeServer { name: "greasy".into(), address: "127.0.0.1:1".into(), public_key: key });
    server.failed(Instant::now(), Duration::from_secs(40), &anyhow!("bad signature"));
    server.answered();
    assert_eq!((logs.warnings("roughtime_server_failing"), logs.count("answering again")), (0, 0));
    assert_eq!((server.failures, server.retry_at), (0, None));
}

#[tokio::test]
async fn a_failing_server_is_backed_off_alerted_once_on_its_third_failure_and_welcomed_back() {
    let (logs, _guard) = Logs::capture();
    let (test_clock, clock) = clocks();
    let good = FakeServer::start(1, VERSION_1, 2, test_clock.clone()).await;
    let silent = FakeServer::start(2, VERSION_1, 2, test_clock.clone()).await;
    silent.answering.store(false, Ordering::SeqCst);
    let mut sampler = sampler(&[&good, &silent], Duration::from_millis(200));

    assert_eq!(sampler.query(&clock).await.unwrap().len(), 1);
    assert_eq!((sampler.servers[0].failures, sampler.servers[1].failures), (0, 1));
    // Backing off: the next round does not ask the silent server.
    sampler.query(&clock).await.unwrap();
    assert_eq!(silent.requests.load(Ordering::SeqCst), 1);

    // A failure or two is normal; the third in a row alerts, once.
    let mut alerts = Vec::new();
    for _ in 0..3 {
        test_clock.advance(sampler.max_retry);
        sampler.query(&clock).await.unwrap();
        alerts.push(logs.warnings("roughtime_server_failing"));
    }
    assert_eq!((sampler.servers[1].failures, alerts), (4, vec![0, 1, 1]));
    // The back-off stops growing at the cap, which leaves a 10s round and a 200ms wait within the
    // grace.
    assert_eq!(sampler.max_retry, Duration::from_millis(49_800));
    while sampler.servers[1].failures < 12 {
        test_clock.advance(sampler.max_retry);
        sampler.query(&clock).await.unwrap();
    }
    assert_eq!(sampler.servers[1].retry_at, Some(clock.instant() + sampler.max_retry));

    silent.answering.store(true, Ordering::SeqCst);
    test_clock.advance(sampler.max_retry);
    assert_eq!(sampler.query(&clock).await.unwrap().len(), 2);
    assert_eq!((sampler.servers[1].failures, sampler.servers[1].retry_at), (0, None));
    assert_eq!(logs.count("roughtime server answering again"), 1);
}

#[tokio::test]
async fn rounds_warn_once_per_outage_and_log_when_samples_return() {
    let (logs, _guard) = Logs::capture();
    let (test_clock, clock) = clocks();
    let servers = honest_servers(&test_clock).await;
    let mut sampler = sampler(&servers.each_ref(), WAIT);
    assert!(sampler.round(&clock).await.unwrap().is_some());

    for outage in 1..=2 {
        servers.iter().for_each(|s| s.garbled.store(true, Ordering::SeqCst));
        for _ in 0..3 {
            assert!(sampler.round(&clock).await.unwrap().is_none());
            test_clock.advance(sampler.max_retry);
        }
        assert_eq!(logs.warnings("trusted_time_no_sample"), outage);
        servers.iter().for_each(|s| s.garbled.store(false, Ordering::SeqCst));
        assert!(sampler.round(&clock).await.unwrap().is_some());
        assert_eq!(logs.count("trusted time sample restored"), outage);
    }
}

#[tokio::test]
async fn a_server_signing_a_time_outside_the_agreement_is_warned_about_once() {
    let (logs, _guard) = Logs::capture();
    let (test_clock, clock) = clocks();
    let servers = honest_servers(&test_clock).await;
    let mut sampler = sampler(&servers.each_ref(), WAIT);
    servers[2].lie_ms.store(30_000, Ordering::SeqCst);
    for _ in 0..2 {
        let sample = sampler.round(&clock).await.unwrap().expect("two honest servers agree");
        assert!(sample.latest_ms < test_clock.true_ms() + 10_000, "{sample:?}");
    }
    let liar = format!("alert=\"roughtime_server_disagrees\" server={}", servers[2].address);
    assert_eq!((logs.warnings("roughtime_server_disagrees"), logs.count(&liar)), (1, 1));

    servers[2].lie_ms.store(0, Ordering::SeqCst);
    sampler.round(&clock).await.unwrap().expect("all three agree");
    assert_eq!(logs.count("roughtime server agrees again"), 1);
}

#[tokio::test]
async fn with_a_server_failing_two_replies_sharing_no_instant_make_no_sample() {
    let (logs, _guard) = Logs::capture();
    let (test_clock, clock) = clocks();
    let servers = honest_servers(&test_clock).await;
    let mut sampler = sampler(&servers.each_ref(), WAIT);
    // With one server failing, the other two, three seconds apart, share no instant.
    servers[2].garbled.store(true, Ordering::SeqCst);
    servers[1].lie_ms.store(3_000, Ordering::SeqCst);
    assert!(sampler.round(&clock).await.unwrap().is_none());
    assert_eq!(logs.warnings("trusted_time_no_sample"), 1);
    assert_eq!(logs.count("no two replies agree on an instant"), 1);

    servers[1].lie_ms.store(0, Ordering::SeqCst);
    assert!(sampler.round(&clock).await.unwrap().is_some());
}

// ── Helpers ──

/// A test clock, and the same clock as the task reads it.
fn clocks() -> (Arc<TestClock>, Arc<dyn Clock>) {
    let clock = Arc::new(TestClock::default());
    (clock.clone(), clock)
}

/// A sampler over `servers` that waits up to `wait` for each reply, which is also its drift
/// bound, sampling every 10 seconds.
fn sampler(servers: &[&FakeServer], wait: Duration) -> Sampler {
    let servers = servers.iter().map(|s| server_config(s)).collect();
    Sampler::new(TimeSection { max_drift: wait, interval: Duration::from_secs(10), servers })
}
