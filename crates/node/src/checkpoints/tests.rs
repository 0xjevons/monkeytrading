//! Signature, fencing, retry, and collateral regression tests.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use engine_core::command::{Command, Sequenced};
use engine_core::types::AccountId;
use engine_core::{Engine, Fixed};
use tokio::sync::mpsc;

use crate::checkpoints::{CheckpointChain, Poster, VaultState, Work, signature};
use crate::log::{Checkpoint, CheckpointDomain, CommandEntry, LogEntry};
use crate::sequencer::{Request, replay};
use crate::tee::EnclaveKey;

#[derive(Clone)]
struct Chain {
    state: Arc<Mutex<VaultState>>,
    fails: Arc<Mutex<usize>>,
    posts: Arc<Mutex<usize>>,
}

impl CheckpointChain for Chain {
    async fn state(&self) -> anyhow::Result<VaultState> {
        Ok(self.state.lock().unwrap().clone())
    }
    async fn post(&self, cp: &Checkpoint) -> anyhow::Result<()> {
        let mut fails = self.fails.lock().unwrap();
        if *fails > 0 {
            *fails -= 1;
            anyhow::bail!("RPC unavailable")
        }
        *self.posts.lock().unwrap() += 1;
        self.state.lock().unwrap().checkpoint = Some((cp.seq, cp.root.clone()));
        Ok(())
    }
}

#[test]
fn strict_replay_rejects_missing_forged_domain_and_signer_signatures() {
    let key = EnclaveKey::generate();
    let mut engine = Engine::new();
    let command = Sequenced { seq: 0, ts: 0, cmd: Command::Tick };
    let events = engine.apply(&command).unwrap();
    let cp = checkpoint(&key, 0, engine.state_root());
    let entries = [
        LogEntry::Command(CommandEntry { sequenced: command, events: Some(events) }),
        LogEntry::Checkpoint(cp.clone()),
    ];
    replay(&entries).unwrap();
    for mutate in [0, 1, 2, 3, 4] {
        let mut altered = cp.clone();
        match mutate {
            0 => altered.signature.clear(),
            1 => altered.signature = "0x".into(),
            2 => altered.signer = EnclaveKey::generate().address_hex(),
            3 => altered.domain.as_mut().unwrap().chain_id += 1,
            _ => altered.domain.as_mut().unwrap().vault = AccountId([8; 20]),
        }
        assert!(replay(&[entries[0].clone(), LogEntry::Checkpoint(altered)]).is_err(), "mutation {mutate}");
    }
    let mut altered = cp;
    altered.signature.replace_range(4..6, "ff");
    assert!(signature::verify(&altered, &engine).is_err());
}

#[tokio::test]
async fn startup_refuses_truncation_wrong_root_and_unrecovered_signer() {
    let (poster, work) = poster();
    poster.startup(None).await.unwrap();
    poster.chain.post(&work.checkpoint).await.unwrap();
    poster.startup(Some(&work.checkpoint)).await.unwrap();
    assert!(poster.startup(None).await.unwrap_err().to_string().contains("shorter"));
    let mut fork = work.checkpoint.clone();
    fork.root = format!("0x{}", "ff".repeat(32));
    assert!(poster.startup(Some(&fork)).await.is_err());
    poster.chain.state.lock().unwrap().signer = AccountId([9; 20]);
    assert!(poster.startup(Some(&work.checkpoint)).await.unwrap_err().to_string().contains("signer"));
}

#[tokio::test]
async fn mismatch_pauses_withdrawals_but_reconciliation_preserves_posting() {
    let (poster, work) = poster();
    assert_eq!(poster.reconcile(&work).await.unwrap(), (Some(5), true));
    poster.chain.state.lock().unwrap().balance -= Fixed::from_int(1);
    assert_eq!(poster.reconcile(&work).await.unwrap(), (Some(5), false));
    poster.chain.state.lock().unwrap().balance += Fixed::from_int(1);
    assert_eq!(poster.reconcile(&work).await.unwrap(), (Some(5), true));
    assert_eq!(*poster.chain.posts.lock().unwrap(), 1, "retry must not repost an included checkpoint");
}

#[tokio::test(start_paused = true)]
async fn failed_post_is_retried_and_never_releases_a_signature_early() {
    let (poster, work) = poster();
    *poster.chain.fails.lock().unwrap() = 1;
    let chain = poster.chain.clone();
    let (work_tx, work_rx) = mpsc::channel(4);
    let (tx, mut rx) = mpsc::channel(4);
    work_tx.send(work).await.unwrap();
    let task = tokio::spawn(poster.run(work_rx, tx));
    assert!(matches!(rx.recv().await, Some(Request::CheckpointStatus { posted: None, healthy: false })));
    assert!(chain.state.lock().unwrap().checkpoint.is_none());
    assert!(matches!(rx.recv().await, Some(Request::CheckpointStatus { posted: Some(5), healthy: true })));
    assert_eq!(*chain.posts.lock().unwrap(), 1);
    drop(work_tx);
    task.await.unwrap().unwrap();
}

#[test]
fn signed_parent_metadata_cannot_be_rewritten_by_the_relayer() {
    let key = EnclaveKey::generate();
    let mut cp = checkpoint(&key, 10, [3; 32]);
    cp.parent = Some(crate::log::CheckpointParent { has_checkpoint: true, seq: 5, root: [7; 32].into() });
    let domain = cp.domain.unwrap();
    cp.signature = key.sign_checkpoint_v2(domain.chain_id, &domain.vault.0, cp.seq, &[3; 32], cp.parent.unwrap());
    signature::verify_domain(&cp, domain).unwrap();
    for mutation in 0..4 {
        let mut altered = cp.clone();
        match mutation {
            0 => altered.parent.as_mut().unwrap().seq += 1,
            1 => altered.parent.as_mut().unwrap().root = [8; 32].into(),
            2 => altered.parent.as_mut().unwrap().has_checkpoint = false,
            _ => altered.parent = None,
        }
        assert!(signature::verify_domain(&altered, domain).is_err(), "mutation {mutation}");
    }
}

#[tokio::test]
async fn stale_parent_fails_closed_before_posting_a_divergent_higher_sequence() {
    let (poster, work) = poster();
    poster.chain.state.lock().unwrap().checkpoint = Some((4, format!("0x{}", "ab".repeat(32))));
    assert!(poster.reconcile(&work).await.unwrap_err().is::<super::Conflict>());
    assert_eq!(*poster.chain.posts.lock().unwrap(), 0);
}

#[tokio::test(start_paused = true)]
async fn uncredited_deposit_mismatch_escalates_after_catch_up_deadline() {
    let (_guard, mut alerts) = crate::test_support::capture_alerts();
    let (poster, work) = poster();
    poster.chain.state.lock().unwrap().deposits += 1;
    let (work_tx, work_rx) = mpsc::channel(4);
    let (tx, mut rx) = mpsc::channel(4);
    work_tx.send(work).await.unwrap();
    let task = tokio::spawn(poster.run(work_rx, tx));
    assert!(matches!(rx.recv().await, Some(Request::CheckpointStatus { healthy: false, .. })));
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(matches!(rx.recv().await, Some(Request::CheckpointStatus { healthy: false, .. })));
    assert!(std::iter::from_fn(|| alerts.try_recv().ok()).any(|alert| alert["alert"] == "collateral_mismatch"));
    drop(work_tx);
    task.await.unwrap().unwrap();
}

fn checkpoint(key: &EnclaveKey, seq: u64, root: [u8; 32]) -> Checkpoint {
    let domain = CheckpointDomain { chain_id: 31337, vault: AccountId([7; 20]) };
    Checkpoint {
        parent: Some(Default::default()),
        seq,
        root: format!("0x{}", hex::encode(root)),
        signer: key.address_hex(),
        signature: key.sign_checkpoint_v2(domain.chain_id, &domain.vault.0, seq, &root, Default::default()),
        domain: Some(domain),
        deferred_withdrawals: true,
    }
}

fn poster() -> (Poster<Chain>, Work) {
    let key = EnclaveKey::generate();
    let checkpoint = checkpoint(&key, 5, [3; 32]);
    let work = Work { checkpoint: checkpoint.clone(), liabilities: Fixed::from_int(100), deposits: 1 };
    let chain = Chain {
        state: Arc::new(Mutex::new(VaultState {
            signer: AccountId(key.address()),
            checkpoint: None,
            balance: work.liabilities,
            deposits: work.deposits,
        })),
        fails: Arc::new(Mutex::new(0)),
        posts: Arc::new(Mutex::new(0)),
    };
    (
        Poster {
            chain,
            domain: checkpoint.domain.unwrap(),
            signer: AccountId(key.address()),
            retry: Duration::from_millis(10),
            pending_deposits_critical_after: Duration::from_secs(1),
        },
        work,
    )
}
