//! Helpers over the events a command returns, shared by the protocol tests.

use engine_core::event::{Event, Fill, Reject};

/// The fills among `events`.
pub fn fills(events: &[Event]) -> Vec<&Fill> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Fill(fill) => Some(fill),
            _ => None,
        })
        .collect()
}

/// The one fill among `events`; panics unless there is exactly one.
pub fn only_fill(events: &[Event]) -> &Fill {
    match fills(events)[..] {
        [fill] => fill,
        _ => panic!("expected one fill, got {events:?}"),
    }
}

/// The order rejection among `events`; panics if there is none.
pub fn rejection(events: &[Event]) -> &Reject {
    events
        .iter()
        .find_map(|e| match e {
            Event::OrderRejected { reason, .. } => Some(reason),
            _ => None,
        })
        .unwrap_or_else(|| panic!("expected a rejection, got {events:?}"))
}
