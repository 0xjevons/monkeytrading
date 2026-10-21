//! Node reference logs: every committed log in `tests/reference/`, in the node's format with each
//! command's events and signed withdrawals, still parses and replays with its events checked. One
//! log is committed per protocol version and never regenerated, so a change to the events a command
//! emits, or to how they serialize, fails here.

use std::fs;
use std::path::PathBuf;

use engine_node::log::{LogEntry, read_file};
use engine_node::replay_path;

#[test]
fn every_node_reference_log_replays_with_its_events() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/reference");
    let mut logs: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    logs.sort();
    assert!(!logs.is_empty(), "no reference logs in {}", dir.display());
    for path in logs {
        let (mut without_events, mut withdrawals) = (0, 0);
        read_file(&path, |entry| {
            match entry {
                LogEntry::Command(c) if c.events.is_none() => without_events += 1,
                LogEntry::Withdrawal(_) => withdrawals += 1,
                _ => {}
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(without_events, 0, "{}: every command carries its events", path.display());
        assert!(withdrawals > 0, "{}: no withdrawal", path.display());
        let replayed = replay_path(&path).unwrap_or_else(|e| panic!("{} no longer replays: {e:#}", path.display()));
        assert!(replayed.checkpoints > 0 && replayed.unsigned.is_empty(), "{}", path.display());
    }
}
