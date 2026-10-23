use super::*;
use std::fs;
use std::io::Write;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use engine_core::command::Command;
use tokio::sync::{broadcast, mpsc, watch};

use crate::api::{Feed, MAX_BACKFILLS};
use crate::config::HistorySection;
use crate::history::{HistoryCheck, PageQuery};
use crate::log::{Manifest, Offset};
use crate::sequencer::{SequencerConfig, replay_path};
use crate::tee::EnclaveKey;
use crate::test_support::{
    RETENTION, TRADER, capture_alerts, deposit, dev_config, logged_history, published_manifest, scratch_dir,
    sequencer_config, trusted_now, withdraw,
};

#[test]
fn snapshot_recovery_matches_full_replay_and_restarts_with_a_new_key() {
    let (dir, cfg, mut seq) = node("snapshot-restart", 8);
    for n in 0..37 {
        seq.stamp(seq.last_ts, deposit(1, &format!("deposit-{n}"))).unwrap();
    }
    seq.flush().unwrap();
    let expected = seq.engine.state_root();
    let tip = seq.last_seq();
    drop(seq);
    let log = recover(&dir, &cfg);
    assert!(log.snapshot_seq.is_some());
    assert_eq!(log.last_seq(), tip);
    let mut full = replay_path(&dir).unwrap();
    let mut recovered = log.replayed;
    assert_eq!(recovered.engine.state_root(), expected);
    assert_eq!(full.engine.state_root(), expected);
    assert_eq!(recovered.activations, full.activations);
    assert_eq!((recovered.commands, recovered.checkpoints), (full.commands, full.checkpoints));
    let config =
        SequencerConfig { snapshots: Some(&cfg), ..sequencer_config(&dir, watch::channel(Manifest::default()).0) };
    let mut restarted = Sequencer::recover(
        config,
        Arc::new(EnclaveKey::generate()),
        broadcast::channel(32).0,
        mpsc::unbounded_channel().0,
    )
    .unwrap();
    restarted.stamp(restarted.last_ts, deposit(7, "after-restart")).unwrap();
    restarted.checkpoint().unwrap();
    restarted.flush().unwrap();
    let expected = restarted.engine.state_root();
    drop(restarted);
    assert_eq!(
        recover(&dir, &cfg).replayed.engine.state_root(),
        expected,
        "untrusted new key falls back to old snapshot and replays suffix"
    );
}

#[test]
fn corrupt_or_truncated_latest_snapshot_warns_and_falls_back_then_genesis() {
    for corrupt in [b"{".as_slice(), b"garbage".as_slice()] {
        let (dir, cfg, mut seq) = node("snapshot-fallback", 4);
        for _ in 0..29 {
            seq.stamp(seq.last_ts, Command::Tick).unwrap();
        }
        seq.checkpoint().unwrap();
        let expected = seq.engine.state_root();
        drop(seq);
        let paths = snapshot::candidates(&cfg).unwrap();
        fs::write(&paths[0], corrupt).unwrap();
        let (_guard, mut alerts) = capture_alerts();
        let mut log = recover(&dir, &cfg);
        assert!(log.snapshot_seq.is_some());
        assert_eq!(log.replayed.engine.state_root(), expected);
        let alert = alerts.try_recv().unwrap();
        assert_eq!(alert["alert"], "snapshot_rejected");
        assert_eq!(alert["level"], "WARN");
        for path in paths {
            fs::write(path, corrupt).unwrap();
        }
        let mut log = recover(&dir, &cfg);
        assert_eq!(log.snapshot_seq, None);
        assert_eq!(log.replayed.engine.state_root(), expected);
    }
}

#[test]
fn missing_or_changed_checkpoint_cannot_authorize_skipping_replay() {
    let (dir, cfg, mut seq) = node("snapshot-missing-checkpoint", 1000);
    seq.stamp(seq.last_ts, Command::Tick).unwrap();
    seq.checkpoint().unwrap();
    let expected = seq.engine.state_root();
    drop(seq);
    for old in snapshot::candidates(&cfg).unwrap().into_iter().skip(1) {
        fs::remove_file(old).unwrap();
    }
    let path = fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| {
            let path = e.unwrap().path();
            (path.extension().is_some_and(|e| e == "jsonl") && path.file_name().unwrap() != "manifest.jsonl")
                .then_some(path)
        })
        .next()
        .unwrap();
    let contents = fs::read_to_string(&path).unwrap();
    let mut lines: Vec<_> = contents.lines().collect();
    lines.pop();
    fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
    let mut recovered = recover(&dir, &cfg);
    assert!(recovered.snapshot_seq.is_none());
    assert_eq!(recovered.replayed.engine.state_root(), expected);
    let mut checkpoint: serde_json::Value = serde_json::from_str(contents.lines().last().unwrap()).unwrap();
    checkpoint["root"] = "0x00".into();
    lines.push("");
    let changed = format!("{}{}\n", lines.join("\n"), serde_json::to_string(&checkpoint).unwrap());
    fs::write(&path, changed).unwrap();
    let error = Sequencer::read_log_with_snapshots(&dir, Some(&cfg), 31337, AccountId([0; 20]))
        .err()
        .expect("a cache cannot hide a changed authoritative checkpoint");
    assert!(format!("{error:#}").contains("state root mismatch"), "{error:#}");
}

#[test]
fn every_physical_suffix_command_is_validated_after_the_checkpoint() {
    for reversed in [false, true] {
        let (dir, cfg, mut seq) = node("snapshot-physical-suffix", 1000);
        seq.stamp(seq.last_ts, Command::Tick).unwrap();
        seq.checkpoint().unwrap();
        seq.flush().unwrap();
        drop(seq);
        let paths = snapshot::candidates(&cfg).unwrap();
        let (snapshot, _) = snapshot::read(&cfg, &paths[0], 31337, AccountId([0; 20])).unwrap();
        let path = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension().is_some_and(|ext| ext == "jsonl") && path.file_name().unwrap() != "manifest.jsonl"
            })
            .max()
            .unwrap();
        let contents = fs::read_to_string(&path).unwrap();
        let command = contents
            .lines()
            .filter_map(|line| match serde_json::from_str::<LogEntry>(line).unwrap() {
                LogEntry::Command(command) => Some(command),
                _ => None,
            })
            .next_back()
            .unwrap();
        let mut forged = command;
        forged.sequenced.seq = snapshot.offset.seq - u64::from(reversed);
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "{}", serde_json::to_string(&LogEntry::Command(forged)).unwrap()).unwrap();
        drop(file);
        assert!(
            Sequencer::read_log_with_snapshots(&dir, Some(&cfg), 31337, AccountId([0; 20])).is_err(),
            "a duplicate or reversed command after the authenticated boundary must fail recovery"
        );
    }
}

/// Isolated dev scenario: command count grows, the state stays fixed, history grows, and startup
/// uses a bounded suffix. Timings are evidence only; assertions enforce the work bound and roots.
#[tokio::test]
async fn snapshot_restart_scenario_includes_history_and_corruption() {
    for commands in [1_000, 10_000] {
        let (dir, cfg, mut seq) = node(&format!("snapshot-scenario-{commands}"), 128);
        let history_cfg = HistorySection { dir: scratch_dir("snapshot-scenario-history"), retention: RETENTION };
        seq.stamp(seq.last_ts, deposit(10, "scenario-deposit")).unwrap();
        for _ in 0..commands {
            seq.stamp(seq.last_ts, withdraw(0)).unwrap();
        }
        // A signed withdrawal is included before the cache boundary, so restart cannot sign twice.
        seq.stamp(seq.last_ts, withdraw(1)).unwrap();
        seq.checkpoint().unwrap();
        let expected = seq.engine.state_root();
        let tip = seq.last_seq();
        drop(seq);
        let history = HistoryCheck::open(&history_cfg).unwrap().finish().unwrap();
        let reader = history.reader();
        let feed = Feed::new(
            broadcast::channel(16).1,
            dir.clone(),
            published_manifest(&dir),
            Arc::new(tokio::sync::Semaphore::new(MAX_BACKFILLS)),
            Some(Offset::start_of(0)),
        );
        history.run(feed).await.unwrap();
        let expected_events = logged_history(&dir, TRADER);
        let paths = snapshot::candidates(&cfg).unwrap();
        for corrupt in [false, true] {
            if corrupt {
                fs::write(&paths[0], b"{\"payload\":").unwrap();
            }
            let start = Instant::now();
            let mut log = recover(&dir, &cfg);
            let mut check = HistoryCheck::open_cursor(&history_cfg).unwrap();
            check.check_log(&dir, log.last_seq()).unwrap();
            let mut history = check.finish().unwrap();
            history.catch_up_to(tip);
            let elapsed = start.elapsed();
            assert!(log.replayed.commands - log.snapshot_seq.unwrap() - 1 <= 128);
            assert_eq!(history.resume_at(), Offset::start_of(tip.unwrap() + 1));
            assert_eq!(log.replayed.engine.state_root(), expected);
            assert!(log.replayed.unsigned.is_empty());
            let mut before = None;
            let mut served = Vec::new();
            loop {
                let query = PageQuery { account: TRADER, kind: None, before, after: None, limit: 200 };
                let page = history.reader().page(query).await.unwrap().unwrap();
                served.extend(page.events);
                before = page.next;
                if before.is_none() {
                    break;
                }
            }
            assert_eq!(served, expected_events, "every retained event survives snapshot recovery exactly once");
            eprintln!(
                "snapshot scenario: commands={commands} corrupt={corrupt} restart_ms={} suffix_commands={}",
                elapsed.as_millis(),
                log.replayed.commands - log.snapshot_seq.unwrap() - 1
            );
        }
        drop(reader);
    }
}

fn recover(dir: &Path, cfg: &SnapshotSection) -> ReplayedLog {
    Sequencer::read_log_with_snapshots(dir, Some(cfg), 31337, AccountId([0; 20])).unwrap()
}

fn node(label: &str, interval: u64) -> (PathBuf, SnapshotSection, Sequencer) {
    let dir = scratch_dir(label);
    let key = Arc::new(EnclaveKey::generate());
    let cfg = SnapshotSection {
        dir: scratch_dir(&format!("{label}-caches")),
        retain: NonZeroUsize::new(3).unwrap(),
        max_bytes: NonZeroUsize::new(64 * 1024 * 1024).unwrap(),
        trusted_signers: vec![AccountId(key.address())],
    };
    let config = SequencerConfig {
        snapshots: Some(&cfg),
        checkpoint_every: interval,
        segment_commands: interval,
        ..sequencer_config(&dir, watch::channel(Manifest::default()).0)
    };
    let mut seq = Sequencer::recover(config, key, broadcast::channel(32).0, mpsc::unbounded_channel().0).unwrap();
    seq.activate_protocol().unwrap();
    seq.genesis(&dev_config().genesis).unwrap();
    seq.trust(trusted_now()).unwrap();
    (dir, cfg, seq)
}
