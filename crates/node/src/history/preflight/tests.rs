//! Read committed WAL data without creating, updating, or deleting original SQLite sidecars.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use engine_core::Engine;
use engine_core::command::{Command, Sequenced};

use crate::config::HistorySection;
use crate::history::INDEX_FILE;
use crate::history::preflight::HistoryPlan;
use crate::history::store::Writer;
use crate::log::{CommandEntry, LogEntry, Offset};
use crate::sequencer::{StreamBody, StreamMsg};
use crate::test_support::{capture_alerts, scratch_dir};

#[test]
fn valid_history_resumes_without_alert_or_byte_changes_for_absent_empty_and_committed_wal() {
    for (wal_kind, with_shm) in [("none", false), ("empty", false), ("frames", false), ("frames", true)] {
        let with_wal = wal_kind == "frames";
        let root = scratch_dir(&format!("history-private-wal-{wal_kind}-{with_shm}"));
        let source = root.join("source");
        let target = root.join("target");
        std::fs::create_dir_all(&target).unwrap();
        let cfg = HistorySection { dir: target.clone(), retention: Duration::from_secs(60) };
        let (writer, _) = Writer::open(&source.join(INDEX_FILE), cfg.retention_ms().unwrap()).unwrap();
        let mut writer = Some(writer);
        let mut engine = Engine::new();
        let mut entries = Vec::new();
        for seq in 0..2 {
            let sequenced = Sequenced { seq, ts: i64::try_from(seq).unwrap(), cmd: Command::Tick };
            let events = engine.apply(&sequenced).unwrap();
            let entry = CommandEntry { sequenced, events: Some(events) };
            writer
                .as_mut()
                .unwrap()
                .write(&[Arc::new(StreamMsg {
                    offset: Offset::start_of(seq),
                    body: StreamBody::Command(entry.clone()),
                })])
                .unwrap();
            entries.push(entry);
        }
        // Copy the idle writer's committed files before its close can checkpoint/delete WAL.
        // This produces a genuine recovery image whose newest progress lives in the WAL.
        let wal = format!("{INDEX_FILE}-wal");
        if with_wal {
            assert!(std::fs::metadata(source.join(&wal)).unwrap().len() > 32);
        } else {
            drop(writer.take());
        }
        for name in [INDEX_FILE.to_owned(), wal, format!("{INDEX_FILE}-shm")] {
            if name == INDEX_FILE || (with_wal && (with_shm || !name.ends_with("-shm"))) {
                std::fs::copy(source.join(&name), target.join(name)).unwrap();
            }
        }
        if wal_kind == "empty" {
            std::fs::write(target.join(format!("{INDEX_FILE}-wal")), []).unwrap();
        }
        let before = files(&target);
        let mut plan = HistoryPlan::open(&cfg).unwrap();
        assert_eq!(plan.progress.as_ref().unwrap().as_ref(), entries.last());
        for entry in &entries {
            plan.observe(&LogEntry::Command(entry.clone()));
        }
        assert_eq!(files(&target), before, "preflight cannot change even existing SHM bytes");
        let (_guard, mut alerts) = capture_alerts();
        let history = plan.finish().unwrap();
        assert_eq!(history.resume_at(), Offset::start_of(2));
        assert!(alerts.try_recv().is_err(), "valid WAL recovery is ordinary startup");
        drop(history);
        drop(writer);
    }
}

fn files(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            (path.file_name().unwrap().to_str().unwrap().to_owned(), std::fs::read(path).unwrap())
        })
        .collect()
}
