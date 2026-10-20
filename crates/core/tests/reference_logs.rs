//! Reference logs: every committed log in `tests/reference/` replays through `Engine::apply` to the
//! state root it recorded after each command. One log is committed per protocol version and never
//! regenerated, so a change that alters any root fails here, naming the first diverging seq.

mod common;

use std::any::Any;
use std::fs;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};

use common::params;
use common::reference::{ReferenceEntry, root_hex};
use engine_core::command::{AdminCommand, Command, Sequenced};
use engine_core::{Engine, Fixed};

#[test]
fn every_reference_log_replays_to_its_committed_roots() {
    let logs = reference_logs();
    assert!(!logs.is_empty(), "no reference logs in {}", reference_dir().display());
    let failures: Vec<String> = logs
        .iter()
        .filter_map(|path| verify_roots(&read(path)).err().map(|e| format!("{}: {e}", name(path))))
        .collect();
    assert!(failures.is_empty(), "reference logs no longer replay to their committed roots:\n{}", failures.join("\n"));
}

#[test]
fn a_changed_root_is_reported_at_its_seq() {
    let mut entries = read(&reference_dir().join("v1.jsonl"));
    entries[30].root = root_hex([0; 32]);
    let err = verify_roots(&entries).unwrap_err();
    assert!(err.starts_with("root diverges at seq 30:"), "{err}");
}

#[test]
fn a_changed_command_is_reported_at_the_first_seq_it_alters() {
    let mut entries = read(&reference_dir().join("v1.jsonl"));
    // One extra unit of the smallest decimal on the first deposit changes every root from there on.
    let seq = entries.iter().position(|e| matches!(e.sequenced.cmd, Command::Deposit { .. })).expect("a deposit");
    let Command::Deposit { amount, .. } = &mut entries[seq].sequenced.cmd else { unreachable!("found above") };
    *amount += Fixed::from_raw(1);
    let err = verify_roots(&entries).unwrap_err();
    assert!(err.starts_with(&format!("root diverges at seq {seq}:")), "{err}");
}

#[test]
fn a_panicking_command_is_reported_at_its_seq() {
    // The rollover after a timestamp past chrono's range cannot be computed, so applying parameters
    // then panics; only a corrupt log carries such a timestamp.
    let cmd = Command::Admin(AdminCommand::SetParams(params()));
    let entries = [ReferenceEntry { sequenced: Sequenced { seq: 0, ts: i64::MAX, cmd }, root: root_hex([0; 32]) }];
    let err = verify_roots(&entries).unwrap_err();
    assert!(err.starts_with("seq 0 panicked: timestamp in range"), "{err}");
}

#[test]
fn an_empty_log_does_not_verify() {
    assert_eq!(verify_roots(&[]), Err("empty log".to_owned()));
}

// ── Helpers ──

/// Replays `entries` on a fresh engine, comparing the state root after every command.
/// Error cases:
/// - no entries
/// - the engine refuses a command's seq or timestamp, or panics applying it
/// - a root differs from the committed one; the message names the first such seq
fn verify_roots(entries: &[ReferenceEntry]) -> Result<(), String> {
    if entries.is_empty() {
        return Err("empty log".into());
    }
    let mut engine = Engine::new();
    entries.iter().try_for_each(|entry| {
        let seq = entry.sequenced.seq;
        // A panic is caught so the failure still names the log and the seq.
        panic::catch_unwind(AssertUnwindSafe(|| engine.apply(&entry.sequenced)))
            .map_err(|payload| format!("seq {seq} panicked: {}", panic_message(payload.as_ref())))?
            .map_err(|e| format!("seq {seq} refused: {e}"))?;
        let replayed = root_hex(engine.state_root());
        if replayed != entry.root {
            return Err(format!("root diverges at seq {seq}: committed {}, replay gives {replayed}", entry.root));
        }
        Ok(())
    })
}

/// The directory holding one reference log per protocol version.
fn reference_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/reference")
}

/// Every file in the reference directory but hidden ones (such as `.DS_Store`), in name order;
/// panics on one that is not a `.jsonl` log, so a misnamed log cannot go unreplayed.
fn reference_logs() -> Vec<PathBuf> {
    let mut logs: Vec<PathBuf> = fs::read_dir(reference_dir())
        .expect("reference directory is readable")
        .map(|entry| entry.expect("directory entry is readable").path())
        .filter(|path| !name(path).starts_with('.'))
        .collect();
    if let Some(stray) = logs.iter().find(|path| !path.is_file() || path.extension().is_none_or(|ext| ext != "jsonl")) {
        panic!("{} is not a .jsonl reference log", stray.display());
    }
    logs.sort();
    logs
}

/// The message a panic carried, when it is a string.
fn panic_message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("")
}

/// The log's file name, which names its protocol version.
fn name(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |name| name.to_string_lossy().into_owned())
}

/// Parses a reference log; panics naming the file and line of the first entry that does not parse.
fn read(path: &Path) -> Vec<ReferenceEntry> {
    let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    text.lines()
        .enumerate()
        .map(|(n, line)| {
            serde_json::from_str(line).unwrap_or_else(|e| panic!("{} line {}: {e}", path.display(), n + 1))
        })
        .collect()
}
