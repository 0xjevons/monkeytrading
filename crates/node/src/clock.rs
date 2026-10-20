//! The node's wall clock. Inside a TEE the host supplies it, so the sequencer only uses it to
//! stamp commands, never to move sequenced time backwards.

use std::time::{SystemTime, UNIX_EPOCH};

use engine_core::types::Millis;

/// Wall-clock milliseconds since the epoch.
pub fn now_ms() -> Millis {
    // The epoch is in the past on any machine that can run the node.
    SystemTime::now().duration_since(UNIX_EPOCH).expect("clock after epoch").as_millis() as Millis
}
