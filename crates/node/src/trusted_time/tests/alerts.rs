//! Trusted time's alerts key on their subject alone and resolve under the same key.

use super::*;

use ed25519_dalek::SigningKey;
use tracing::subscriber::set_default;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::filter_fn;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry;

use crate::alerts::{Routes, channel, is_alert_callsite};
use crate::metrics::Metrics;
use crate::trusted_time::gate::TimeGate;

/// True time when the samples are taken.
const TRUE_MS: Millis = 1_792_502_815_000;

/// Whatever the readings, counts, or reasons, each alert keeps one key per subject, so its repeats
/// share an episode, and the line that ends the condition resolves it under that key.
#[tokio::test]
async fn trusted_time_alerts_keep_one_key_per_subject_and_resolve_under_it() {
    crate::test_support::keep_callsites_interested();
    let (layer, router) = channel(Routes::default(), Arc::new(Metrics::new()));
    let _logging = set_default(registry().with(layer.with_filter(filter_fn(is_alert_callsite))));
    let now = Instant::now();
    let around = |at: Instant| Sample { at, earliest_ms: TRUE_MS - 3_000, latest_ms: TRUE_MS + 3_000 };

    // The gate: a drift halt, a no-sample halt in its place, then a stamp back within the bound.
    let mut gate = TimeGate::new(Duration::from_secs(10), now);
    gate.update(around(now));
    assert!(!gate.admit(TRUE_MS + 60_000, TRUE_MS + 60_000, now));
    assert!(!gate.admit(TRUE_MS + 70_000, TRUE_MS + 70_000, now));
    let stale = now + NO_SAMPLE_GRACE + Duration::from_secs(1);
    assert!(!gate.admit(TRUE_MS, TRUE_MS, stale));
    gate.update(around(stale));
    assert!(gate.admit(TRUE_MS, TRUE_MS, stale));

    // A server: failing past the threshold then answering, and disagreeing then agreeing.
    let key = SigningKey::from_bytes(&[7; 32]).verifying_key();
    let config = RoughtimeServer { name: "int08h".into(), address: "localhost:2002".into(), public_key: key };
    let mut server = Server::new(config);
    for _ in 0..=FAILURES_BEFORE_ALERT {
        server.failed(now, Duration::from_secs(1), &anyhow!("timed out"));
    }
    server.answered();
    let sample = Sample { at: now, earliest_ms: TRUE_MS - 1_000, latest_ms: TRUE_MS + 1_000 };
    for (earliest_ms, latest_ms) in [(TRUE_MS + 50_000, TRUE_MS + 51_000), (TRUE_MS + 60_000, TRUE_MS + 61_000)] {
        server.compare(&Reply { received: now, earliest_ms, latest_ms }, &sample);
    }
    server.compare(&Reply { received: now, earliest_ms: TRUE_MS, latest_ms: TRUE_MS }, &sample);

    // The sampler: a round with every reply garbled makes no sample, and the next good one does.
    let test_clock = Arc::new(TestClock::default());
    let clock: Arc<dyn Clock> = test_clock.clone();
    let servers = honest_servers(&test_clock).await;
    let configs = servers.iter().map(server_config).collect();
    let mut sampler = Sampler::new(TimeSection {
        max_drift: Duration::from_secs(5),
        interval: Duration::from_secs(10),
        servers: configs,
    });
    servers.iter().for_each(|s| s.garbled.store(true, Ordering::SeqCst));
    assert!(sampler.round(&clock).await.unwrap().is_none());
    servers.iter().for_each(|s| s.garbled.store(false, Ordering::SeqCst));
    test_clock.advance(sampler.max_retry);
    assert!(sampler.round(&clock).await.unwrap().is_some());

    let lines: Vec<(String, bool)> = router.queued().iter().map(|line| (line.key(), line.severity.is_none())).collect();
    let fired = |key: &str| (key.to_owned(), false);
    let resolved = |key: &str| (key.to_owned(), true);
    assert_eq!(
        lines,
        [
            fired("clock_drift{}"),
            resolved("clock_drift{}"),
            fired("trusted_time_unavailable{}"),
            resolved("trusted_time_unavailable{}"),
            fired("roughtime_server_failing{server=int08h}"),
            resolved("roughtime_server_failing{server=int08h}"),
            fired("roughtime_server_disagrees{server=int08h}"),
            resolved("roughtime_server_disagrees{server=int08h}"),
            fired("trusted_time_no_sample{}"),
            resolved("trusted_time_no_sample{}"),
        ]
    );
}
