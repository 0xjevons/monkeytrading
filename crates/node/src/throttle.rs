//! Alert throttling, for alerts whose cause can recur many times a minute.

use std::mem::take;
use std::time::Duration;

use tokio::time::Instant;

/// After an alert fires, the same alert waits this long before it fires again, so a cause that
/// keeps recurring raises one alert a minute, not one per occurrence.
pub(crate) const REALERT_AFTER: Duration = Duration::from_secs(60);

/// Holds an alert to one per `REALERT_AFTER`, counting the ones it held back.
#[derive(Default)]
pub(crate) struct Throttle {
    last: Option<Instant>,
    held: u64,
}

impl Throttle {
    /// Whether to alert at `now`, and if so, how many alerts it held back since the last one.
    pub(crate) fn ready(&mut self, now: Instant) -> Option<u64> {
        if self.last.is_some_and(|at| now.duration_since(at) < REALERT_AFTER) {
            self.held += 1;
            return None;
        }
        self.last = Some(now);
        Some(take(&mut self.held))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
