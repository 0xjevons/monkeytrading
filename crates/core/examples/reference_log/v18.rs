//! Dated benchmark catch-up for V18.

use chrono::Duration;
use engine_core::benchmark::{BenchmarkBatch, BenchmarkRate};
use engine_core::command::AdminCommand;
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;

use crate::common::{Harness, f};

/// Activates V18 and records two missing rollovers with distinct published effective dates.
pub(crate) fn session(h: &mut Harness) {
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V18 });
    let first = h.engine.next_rollover().expect("the session has parameters");
    let second = h.engine.params().unwrap().rollover.next_after(first);
    h.advance_to(second + 1);
    let first_date = h.engine.benchmark_date(first);
    let second_date = h.engine.benchmark_date(second);
    let batch = BenchmarkBatch {
        as_of: h.engine.benchmark_date(h.ts),
        checked: h.engine.benchmark_state().required,
        rates: vec![
            BenchmarkRate {
                benchmark: "SOFR".into(),
                effective_date: first_date - Duration::days(1),
                annual_rate: f("0.04"),
            },
            BenchmarkRate { benchmark: "SOFR".into(), effective_date: second_date, annual_rate: f("0.03997") },
        ],
    };
    let events = h.operate(AdminCommand::SetBenchmarkRates(batch));
    assert!(events.contains(&Event::AdminApplied), "{events:?}");
    for date in [first_date, second_date] {
        assert!(events.iter().any(|e| matches!(e, Event::BenchmarkRateObserved { benchmark, rollover_date, .. } if benchmark == "SOFR" && *rollover_date == date)), "{events:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dated_reference_session_covers_two_rollovers_without_writing_a_fixture() {
        let mut h = Harness::v17();
        session(&mut h);
        assert_eq!(h.engine.protocol_version(), ProtocolVersion::V18);
        assert_eq!(h.engine.benchmark_history()["SOFR"].values().copied().collect::<Vec<_>>(), [f("0.03997")]);
    }
}
