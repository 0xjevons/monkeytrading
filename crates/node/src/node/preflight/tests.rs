//! Refused startup must not create, checkpoint, discard, or modify persisted history/log bytes.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use engine_core::Fixed;
use engine_core::types::AccountId;

use crate::checkpoints::{CheckpointChain, Poster, VaultState, View};
use crate::history::HistoryPlan;
use crate::log::CheckpointDomain;
use crate::node::preflight::finish_preflight;
use crate::tee::EnclaveKey;
use crate::test_support::{dev_config, scratch_dir};

struct Chain {
    state: VaultState,
}
impl CheckpointChain for Chain {
    async fn state(&self, _: View) -> anyhow::Result<VaultState> {
        Ok(self.state.clone())
    }
    async fn post(&self, _: &crate::log::Checkpoint) -> anyhow::Result<()> {
        unreachable!()
    }
}

#[tokio::test]
async fn rejected_anchor_and_signer_preserve_missing_corrupt_and_wal_history_bytes() {
    for history_kind in 0..3 {
        for signer_mismatch in [false, true] {
            let root = scratch_dir(&format!("preflight-bytes-{history_kind}-{signer_mismatch}"));
            std::fs::create_dir_all(&root).unwrap();
            let mut cfg = dev_config();
            cfg.node.log_dir = root.join("log");
            cfg.history.dir = root.join("history");
            std::fs::create_dir_all(&cfg.node.log_dir).unwrap();
            std::fs::write(cfg.node.log_dir.join("manifest.jsonl"), b"{\"interrupted\":").unwrap();
            let mut conn = None;
            if history_kind > 0 {
                std::fs::create_dir_all(&cfg.history.dir).unwrap();
                let path = cfg.history.dir.join(crate::history::INDEX_FILE);
                if history_kind == 1 {
                    std::fs::write(path, b"corrupt diagnostic bytes").unwrap();
                } else {
                    let db = rusqlite::Connection::open(path).unwrap();
                    db.execute_batch("PRAGMA journal_mode=wal; CREATE TABLE diagnostic(value TEXT); INSERT INTO diagnostic VALUES('preserve WAL');").unwrap();
                    conn = Some(db);
                }
            }
            let before = files(&root);
            let key = EnclaveKey::generate();
            let poster = Poster {
                chain: Chain {
                    state: VaultState {
                        signer: if signer_mismatch { AccountId([9; 20]) } else { AccountId(key.address()) },
                        checkpoint: Some((9, format!("0x{}", "ab".repeat(32)))),
                        balance: Fixed::ZERO,
                        deposits: 0,
                    },
                },
                domain: CheckpointDomain { chain_id: 31337, vault: AccountId([0; 20]) },
                signer: AccountId(key.address()),
                retry: Duration::from_millis(1),
                pending_deposits_critical_after: Duration::from_secs(1),
            };
            let plan = HistoryPlan::open(&cfg.history).unwrap();
            assert!(finish_preflight(plan, Some((&poster, None))).await.is_err());
            assert_eq!(files(&root), before, "history kind {history_kind}, signer mismatch {signer_mismatch}");
            drop(conn);
        }
    }
}

#[tokio::test]
async fn replay_valid_truncated_log_cannot_discard_a_newer_valid_history_index() {
    let root = scratch_dir("preflight-truncated-valid-index");
    std::fs::create_dir_all(&root).unwrap();
    let mut cfg = dev_config();
    let full = root.join("full-log");
    cfg.history.dir = root.join("history");
    let node = crate::test_support::TestSequencer::start(&full, 1024);
    let history = crate::test_support::TestHistory::start(&node, &full, &cfg.history.dir).await.unwrap();
    node.submit(crate::test_support::deposit(100, "startup-boundary")).await;
    node.stop().await;
    history.finish().await.unwrap();
    let entries = crate::test_support::log_entries(&full);
    let anchor = entries
        .iter()
        .rev()
        .find_map(|entry| if let crate::log::LogEntry::Checkpoint(cp) = entry { Some(cp.clone()) } else { None })
        .unwrap();
    cfg.node.log_dir = root.join("short-log");
    std::fs::create_dir_all(&cfg.node.log_dir).unwrap();
    let short = entries
        .iter()
        .filter(|entry| match entry {
            crate::log::LogEntry::Command(command) => command.sequenced.seq < anchor.seq,
            crate::log::LogEntry::Checkpoint(cp) => cp.seq < anchor.seq,
            _ => true,
        })
        .map(|entry| format!("{}\n", serde_json::to_string(entry).unwrap()))
        .collect::<String>();
    std::fs::write(cfg.node.log_dir.join("00000000000000000000.jsonl"), short).unwrap();
    let before = files(&root);
    let key = EnclaveKey::test_key();
    let mut found = None;
    let (_log, plan) = crate::node::read_log_and_history_observe(&cfg, Some(AccountId(key.address())), |entry| {
        if let crate::log::LogEntry::Checkpoint(cp) = entry
            && cp.seq == anchor.seq
        {
            found = Some(cp.clone());
        }
    })
    .unwrap();
    let poster = Poster {
        chain: Chain {
            state: VaultState {
                signer: AccountId(key.address()),
                checkpoint: Some((anchor.seq, anchor.root)),
                balance: Fixed::from_int(100),
                deposits: 1,
            },
        },
        domain: anchor.domain.unwrap(),
        signer: AccountId(key.address()),
        retry: Duration::from_millis(1),
        pending_deposits_critical_after: Duration::from_secs(1),
    };
    assert!(finish_preflight(plan, Some((&poster, found.as_ref()))).await.is_err());
    assert_eq!(files(&root), before);
}

fn files(root: &Path) -> BTreeMap<std::path::PathBuf, Vec<u8>> {
    let mut result = BTreeMap::new();
    for entry in std::fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            result.extend(files(&path));
        } else {
            result.insert(path.clone(), std::fs::read(path).unwrap());
        }
    }
    result
}
