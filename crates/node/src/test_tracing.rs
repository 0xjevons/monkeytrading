//! The tracing setup the test collectors share: one process-wide interested subscriber, so no
//! callsite can be cached uninteresting and a collector never silently misses an alert or line.

use std::iter;
use std::sync::Once;

use tracing::Dispatch;
use tracing::subscriber::set_global_default;
use tracing::warn;
use tracing_subscriber::Registry;

use crate::test_support::capture_alerts;

/// Installs once per process the interested subscriber every alert and log collector relies on.
/// The first hit of a callsite caches its interest process-wide, and a bare thread — one running
/// a test without a collector — can cache it uninteresting so collectors miss it silently. One
/// permanently registered dispatch keeps tracing's interest computed from all dispatchers, never
/// none; the global default answers the single-dispatcher path, which asks the hitting thread.
/// Both are bare `Registry`s: they emit nothing and never replace a thread's collector.
pub fn keep_callsites_interested() {
    static INTERESTED: Once = Once::new();
    INTERESTED.call_once(|| {
        std::mem::forget(Dispatch::new(Registry::default()));
        let _ = set_global_default(Registry::default());
    });
}

/// An alert whose callsite a thread with no collector hits first must still reach a collector on
/// another thread. `capture_alerts` installs the process-wide interested subscriber; without it,
/// the bare hit caches the probe callsite as uninteresting and the collector misses the alert.
/// The probe runs alone in a child process, where that is deterministic: beside parallel
/// collectors in this process, their registered dispatchers would keep the probe callsite
/// interested even with the install removed.
#[test]
fn an_alert_raised_first_without_a_collector_still_reaches_one() {
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "test_tracing::bare_alert_probe", "--include-ignored"])
        .status()
        .unwrap();
    assert!(status.success(), "the probe failed: a bare first hit dropped the alert");
}

/// The probe body, run alone in a child process by
/// `an_alert_raised_first_without_a_collector_still_reaches_one`.
#[test]
#[ignore = "run alone by an_alert_raised_first_without_a_collector_still_reaches_one"]
fn bare_alert_probe() {
    fn probe() {
        warn!(alert = "alert_capture_probe", "probe");
    }
    let (_guard, mut alerts) = capture_alerts();
    // The bare thread's hit is the probe callsite's first in the child process, the one a bare
    // process caches as uninteresting.
    std::thread::spawn(probe).join().unwrap();
    probe();
    let raised = iter::from_fn(|| alerts.try_recv().ok()).collect::<Vec<_>>();
    assert_eq!(raised.len(), 1, "{raised:?}");
    assert_eq!(raised[0]["alert"], "alert_capture_probe");
}
