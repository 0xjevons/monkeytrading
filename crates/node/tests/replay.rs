//! A log the node wrote before protocol versions existed still replays, through `engine replay`'s
//! log format and checkpoints, to the roots it was signed with. Each version's engine roots are
//! pinned by `crates/core/tests/reference/`.

use std::path::Path;

use engine_core::protocol::ProtocolVersion;
use engine_node::{log, replay};

/// Written by the node before protocol versions existed (commit 452ec1b), on `config/dev.toml`
/// with two mock-priced markets and `checkpoint_every = 40`: genesis, prices and ticks, a deposit,
/// a market and a limit order, a cancel, a withdrawal, and three checkpoints.
const V1_DEV: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/v1-dev.jsonl");

#[test]
fn a_log_written_before_protocol_versions_replays_to_its_roots() {
    let replayed = replay(&log::read_all(Path::new(V1_DEV)).unwrap()).unwrap();
    assert_eq!((replayed.commands, replayed.checkpoints), (63, 3));
    assert_eq!(replayed.engine.protocol_version(), ProtocolVersion::V2);
    assert!(replayed.activations.is_empty());
    // The root that build printed for this log.
    let root = format!("0x{}", hex::encode(replayed.engine.state_root()));
    assert_eq!(root, "0xeb7239777dc18152f4e4a75cd902800dbb918b1e6da35da6171d391d649df535");
}
