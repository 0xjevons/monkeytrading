//! Diagnostic evidence must remain valid independently of transport and backend lag.

use crate::custody::evm::{VaultIdentity, VaultReader};
use crate::custody::stall::Polled;
use crate::custody::tests::fakes::*;
use crate::custody::tests::quorum::{chains, quorum, watcher};

#[tokio::test]
async fn header_content_contradictions_remove_support_but_transport_failures_preserve_it() {
    let chains = chains();
    let quorum = quorum(&chains).await;
    for (i, chain) in chains.iter().enumerate() {
        let mut state = chain.0.lock().unwrap();
        state.head = [10, 11, 999][i];
        state.head_time = Some([100, 101, 999_999][i]);
    }
    for bad in [BadHeaders::Missing, BadHeaders::Empty, BadHeaders::WrongHeight, BadHeaders::Extra] {
        chains[2].0.lock().unwrap().bad_headers = Some(bad);
        let f = quorum.finality().await.unwrap();
        assert_eq!((f.head, f.head_time), (10, 100));
        assert_eq!(f.head_support.unwrap().heads, vec![Some(10), Some(11), None]);
        assert_eq!(f.tagged.unwrap().hash, canonical_hash(1));
    }
    chains[2].0.lock().unwrap().bad_headers = None;
    chains[2].0.lock().unwrap().header_failures = 100;
    let f = quorum.finality().await.unwrap();
    assert_eq!((f.head, f.head_time), (11, 101));
    assert_eq!(f.head_support.unwrap().heads, vec![Some(10), Some(11), Some(999)]);
    assert_eq!(f.tagged.unwrap().hash, canonical_hash(1));
}

#[tokio::test(start_paused = true)]
async fn alternating_lagging_backends_cannot_erase_a_providers_demonstrated_advance() {
    let dir = scratch("quorum-retained-progress");
    let node = Node::start(&dir);
    let chains = chains();
    let mut watcher = watcher(&chains, &dir);
    let (logs, _guard) = Logs::capture();
    for (chain, head) in chains.iter().zip([10, 100, 1]) {
        chain.0.lock().unwrap().head = head;
    }
    chains[2].0.lock().unwrap().head_time = Some(u64::MAX);
    assert!(matches!(watcher.poll(&node.tx).await, Ok(Polled::Read { .. })));
    for (chain, head) in chains.iter().zip([11, 100, 1]) {
        chain.0.lock().unwrap().head = head;
    }
    assert!(matches!(watcher.poll(&node.tx).await, Ok(Polled::Waiting)));
    for (chain, head) in chains.iter().zip([10, 101, 1]) {
        chain.0.lock().unwrap().head = head;
    }
    assert!(matches!(watcher.poll(&node.tx).await, Ok(Polled::Waiting)));
    tokio::time::advance(std::time::Duration::from_secs(2)).await;
    for (chain, head) in chains.iter().zip([11, 100, 1]) {
        chain.0.lock().unwrap().head = head;
    }
    assert!(matches!(watcher.poll(&node.tx).await, Ok(Polled::Waiting)));
    assert_eq!(logs.lines_with("alert=\"custody_finality_stalled\"").len(), 1);
}

#[tokio::test(start_paused = true)]
async fn retained_progress_needs_current_qualification_and_resets_after_finality_advances() {
    let dir = scratch("quorum-retained-qualification");
    let node = Node::start(&dir);
    let chains = chains();
    let mut watcher = watcher(&chains, &dir);
    let (logs, _guard) = Logs::capture();
    for (chain, head) in chains.iter().zip([10, 100, 1]) {
        chain.0.lock().unwrap().head = head;
    }
    assert!(matches!(watcher.poll(&node.tx).await, Ok(Polled::Read { .. })));
    chains[0].0.lock().unwrap().head = 11;
    assert!(matches!(watcher.poll(&node.tx).await, Ok(Polled::Waiting)));
    chains[0].0.lock().unwrap().identity = Some(VaultIdentity { chain_id: CHAIN_ID + 1, token: Some(USDC_TOKEN) });
    chains[1].0.lock().unwrap().head = 101;
    assert!(matches!(watcher.poll(&node.tx).await, Ok(Polled::Waiting)));
    tokio::time::advance(std::time::Duration::from_secs(2)).await;
    assert!(matches!(watcher.poll(&node.tx).await, Ok(Polled::Waiting)));
    assert_eq!(logs.lines_with("alert=\"custody_finality_stalled\"").len(), 0);
    chains[0].0.lock().unwrap().identity = None;
    chains[0].0.lock().unwrap().head = 10;
    assert!(matches!(watcher.poll(&node.tx).await, Ok(Polled::Waiting)));
    tokio::time::advance(std::time::Duration::from_secs(2)).await;
    assert!(matches!(watcher.poll(&node.tx).await, Ok(Polled::Waiting)));
    assert_eq!(logs.lines_with("alert=\"custody_finality_stalled\"").len(), 1);
    for chain in &chains {
        chain.set_final(Some(2));
        let mut state = chain.0.lock().unwrap();
        state.head = state.head.max(2);
    }
    assert!(matches!(watcher.poll(&node.tx).await, Ok(Polled::Read { .. })));
    assert_eq!(logs.lines_with("alert_resolved=\"custody_finality_stalled\"").len(), 1);
    assert!(matches!(watcher.poll(&node.tx).await, Ok(Polled::Waiting)));
    tokio::time::advance(std::time::Duration::from_secs(2)).await;
    assert!(matches!(watcher.poll(&node.tx).await, Ok(Polled::Waiting)));
    assert_eq!(logs.lines_with("alert=\"custody_finality_stalled\"").len(), 1);
}
