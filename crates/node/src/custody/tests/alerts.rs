//! The watcher's alerts over time: one per kind of stall, escalation of a stall that lasts, and
//! the finality and head watchdogs. Paused time drives the poll loop.

use std::time::Duration;

use engine_core::Engine;
use tokio::sync::oneshot;
use tokio::time::{sleep, timeout};

use crate::custody::evm::{Beneficiary, VaultIdentity};
use crate::custody::stall::Polled;
use crate::custody::tests::fakes::{
    FakeChain, Logs, Node, TRADER, USDC, USDC_TOKEN, deposit, refusing_sequencer, scratch, stored_cursor, usdc,
    wait_for_polls, watcher,
};
use crate::sequencer::Request;

#[tokio::test(start_paused = true)]
async fn an_rpc_outage_alerts_once_escalates_once_and_crediting_resumes_after_it() {
    let (logs, _guard) = Logs::capture();
    let dir = scratch("outage");
    let (chain, node) = (FakeChain::default(), Node::start(&dir));
    chain.add(deposit(0, 1, Beneficiary::Trader(TRADER), 3 * USDC));
    chain.set_final(Some(1));
    chain.0.lock().unwrap().failing = true;
    let run = tokio::spawn(watcher(&chain, &dir, 1_000, None).run(node.tx.clone()));

    while chain.0.lock().unwrap().polls < 5 {
        sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(node.balance(TRADER).await, None);
    assert_eq!(logs.count("alert=\"custody_rpc_failed\""), 1);
    assert_eq!(logs.count("alert=\"custody_deposits_stalled\""), 0);
    // The outage outlasts `stall_critical_after` (1s) and escalates, once, naming the cause.
    sleep(Duration::from_millis(1_500)).await;
    let escalated = logs.lines_with("alert=\"custody_deposits_stalled\"");
    assert_eq!(escalated.len(), 1, "{escalated:?}");
    assert!(escalated[0].contains("connection refused"), "{escalated:?}");
    assert_eq!(logs.count("alert=\"custody_rpc_failed\""), 1);
    chain.0.lock().unwrap().failing = false;
    while node.balance(TRADER).await.is_none() {
        sleep(Duration::from_millis(5)).await;
    }

    assert_eq!(node.balance(TRADER).await, Some(usdc("3")));
    for _ in 0..100 {
        if logs.count("deposit watcher resumed") > 0 {
            break;
        }
        sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(logs.count("deposit watcher resumed"), 1);
    run.abort();
}

#[tokio::test(start_paused = true)]
async fn each_kind_of_stall_raises_its_own_alert_at_its_own_level() {
    let (logs, _guard) = Logs::capture();
    let dir = scratch("stall-kinds");
    let (chain, node) = (FakeChain::default(), Node::start(&dir));
    chain.add(deposit(0, 3, Beneficiary::Trader(TRADER), USDC));
    chain.set_final(Some(5));
    chain.0.lock().unwrap().identity = Some(VaultIdentity { chain_id: 1, token: Some(USDC_TOKEN) });
    let run = tokio::spawn(watcher(&chain, &dir, 1_000, None).run(node.tx.clone()));
    wait_for_polls(&chain, 5).await;

    let inconsistent = logs.lines_with("alert=\"custody_chain_inconsistent\"");
    assert_eq!(inconsistent.len(), 1, "{inconsistent:?}");
    assert!(inconsistent[0].contains("ERROR"), "{inconsistent:?}");

    // The stall changes kind: the endpoint is right now, but a lagging backend hides the logs.
    {
        let mut state = chain.0.lock().unwrap();
        state.identity = None;
        state.hide_logs = true;
    }
    wait_for_polls(&chain, 10).await;
    let unsettled = logs.lines_with("alert=\"custody_chain_unsettled\"");
    assert_eq!(unsettled.len(), 1, "{unsettled:?}");
    assert!(unsettled[0].contains("WARN"), "{unsettled:?}");
    assert_eq!(logs.count("alert=\"custody_chain_inconsistent\""), 1);
    run.abort();
}

#[tokio::test(start_paused = true)]
async fn a_final_block_that_stops_while_the_head_moves_raises_one_critical_alert() {
    let (logs, _guard) = Logs::capture();
    let dir = scratch("finality-stall");
    let (chain, node) = (FakeChain::default(), Node::start(&dir));
    chain.set_final(Some(5));
    chain.0.lock().unwrap().head = 8;
    let run = tokio::spawn(watcher(&chain, &dir, 1_000, None).run(node.tx.clone()));

    // An idle chain: neither the head nor the final block moves, which is no stall.
    sleep(Duration::from_millis(1_500)).await;
    assert_eq!(logs.count("alert=\"custody_finality_stalled\""), 0);

    // The head moves on while the final block stays at 5. The idle second does not count: the
    // first moves get the whole budget.
    for _ in 0..5 {
        chain.0.lock().unwrap().head += 1;
        sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(logs.count("alert=\"custody_finality_stalled\""), 0);
    // It keeps moving past `stall_critical_after` (1s).
    for _ in 0..25 {
        chain.0.lock().unwrap().head += 1;
        sleep(Duration::from_millis(100)).await;
    }
    let stalled = logs.lines_with("alert=\"custody_finality_stalled\"");
    assert_eq!(stalled.len(), 1, "{stalled:?}");
    assert!(stalled[0].contains("ERROR"), "{stalled:?}");

    chain.set_final(Some(30));
    let polls = chain.0.lock().unwrap().polls;
    wait_for_polls(&chain, polls + 2).await;
    assert_eq!(logs.count("the final block is advancing again"), 1);
    run.abort();
}

#[tokio::test]
async fn a_stale_endpoint_head_raises_one_critical_alert() {
    let (logs, _guard) = Logs::capture();
    let dir = scratch("stale-head");
    let (chain, node) = (FakeChain::default(), Node::start(&dir));
    chain.set_final(Some(5));
    chain.0.lock().unwrap().head_time = Some(0);
    let mut watcher = watcher(&chain, &dir, 1_000, None);

    assert!(matches!(watcher.poll(&node.tx).await.map(Polled::caught_up), Ok(true)));
    assert!(matches!(watcher.poll(&node.tx).await.map(Polled::caught_up), Ok(true)));
    let stale = logs.lines_with("alert=\"custody_head_stale\"");
    assert_eq!(stale.len(), 1, "{stale:?}");
    assert!(stale[0].contains("ERROR"), "{stale:?}");

    chain.0.lock().unwrap().head_time = None;
    assert!(matches!(watcher.poll(&node.tx).await.map(Polled::caught_up), Ok(true)));
    assert_eq!(logs.count("the endpoint's head is current again"), 1);
}

#[tokio::test(start_paused = true)]
async fn a_final_block_that_only_steps_back_and_forth_still_raises_the_alert() {
    let (logs, _guard) = Logs::capture();
    let dir = scratch("finality-flap");
    let (chain, node) = (FakeChain::default(), Node::start(&dir));
    chain.set_final(Some(5));
    chain.0.lock().unwrap().head = 8;
    let run = tokio::spawn(watcher(&chain, &dir, 1_000, None).run(node.tx.clone()));

    // Replicas alternate between final blocks 4 and 5 while the head moves on: no new high.
    for step in 0..30 {
        {
            let mut state = chain.0.lock().unwrap();
            state.head += 1;
            state.final_block = Some(if step % 2 == 0 { 4 } else { 5 });
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(logs.count("alert=\"custody_finality_stalled\""), 1);
    run.abort();
}

#[tokio::test]
async fn a_stale_backend_among_current_ones_raises_nothing() {
    let (logs, _guard) = Logs::capture();
    let dir = scratch("head-flap");
    let (chain, node) = (FakeChain::default(), Node::start(&dir));
    chain.set_final(Some(5));
    let mut watcher = watcher(&chain, &dir, 1_000, None);

    // Answers alternate between a current backend and one stuck an hour back.
    for step in 0..6 {
        chain.0.lock().unwrap().head_time = (step % 2 == 1).then_some(0);
        watcher.poll(&node.tx).await.ok();
    }
    assert_eq!(logs.count("alert=\"custody_head_stale\""), 0);
}

#[tokio::test(start_paused = true)]
async fn polls_that_only_wait_neither_end_a_stall_nor_stop_its_clock() {
    let (logs, _guard) = Logs::capture();
    let dir = scratch("stall-waiting");
    let (chain, node) = (FakeChain::default(), Node::start(&dir));
    chain.add(deposit(0, 3, Beneficiary::Trader(TRADER), USDC));
    chain.0.lock().unwrap().hide_logs = true;
    let run = tokio::spawn(watcher(&chain, &dir, 1_000, None).run(node.tx.clone()));

    // A lagging backend with no final block alternates with one whose log answer is short.
    for step in 0..150 {
        chain.set_final((step % 2 == 0).then_some(5));
        sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(logs.count("alert=\"custody_chain_unsettled\""), 1);
    assert_eq!(logs.count("alert=\"custody_deposits_stalled\""), 1);
    assert_eq!(logs.count("deposit watcher resumed"), 0);
    run.abort();
}

#[tokio::test(start_paused = true)]
async fn reads_that_keep_trailing_the_final_block_raise_one_critical_alert() {
    let (logs, _guard) = Logs::capture();
    let dir = scratch("lagging");
    let (chain, node) = (FakeChain::default(), Node::start(&dir));
    // Ten blocks per read at 100ms each: 2,000 blocks take far longer than the 1s budget.
    chain.set_final(Some(2_000));
    chain.0.lock().unwrap().latency = Some(Duration::from_millis(100));
    let run = tokio::spawn(watcher(&chain, &dir, 10, None).run(node.tx.clone()));

    sleep(Duration::from_millis(1_500)).await;
    let lagging = logs.lines_with("alert=\"custody_deposits_lagging\"");
    assert_eq!(lagging.len(), 1, "{lagging:?}");
    assert!(lagging[0].contains("ERROR"), "{lagging:?}");
    sleep(Duration::from_millis(1_500)).await;
    assert_eq!(logs.count("alert=\"custody_deposits_lagging\""), 1);
    run.abort();
}

#[tokio::test(start_paused = true)]
async fn a_proof_that_holds_back_every_read_raises_the_lag_alert() {
    let (logs, _guard) = Logs::capture();
    let dir = scratch("long-proof");
    let (chain, node) = (FakeChain::default(), Node::start(&dir));
    // Ten headers per batch at 100ms each: proving 2,000 blocks before the first read takes far
    // longer than the 1s budget.
    chain.set_final(Some(2_000));
    chain.0.lock().unwrap().header_latency = Some(Duration::from_millis(100));
    let run = tokio::spawn(watcher(&chain, &dir, 10, None).run(node.tx.clone()));

    sleep(Duration::from_millis(1_500)).await;
    assert!(chain.reads().is_empty(), "no deposit is read while the chain is proven");
    assert_eq!(logs.count("alert=\"custody_deposits_lagging\""), 1);
    run.abort();
}

#[tokio::test(start_paused = true)]
async fn a_deposit_whose_stamp_the_sequencer_refuses_is_retried_without_an_alert() {
    let (logs, _guard) = Logs::capture();
    let dir = scratch("refused-stamp");
    let chain = FakeChain::default();
    chain.add(deposit(0, 1, Beneficiary::Trader(TRADER), USDC));
    chain.set_final(Some(1));
    let tx = refusing_sequencer(true, 3);
    let run = tokio::spawn(watcher(&chain, &dir, 1_000, None).run(tx.clone()));

    // A stopped watcher polls no more, so the wait times out.
    timeout(Duration::from_secs(5), wait_for_polls(&chain, 20)).await.expect("the watcher keeps polling");
    let (reply, balance) = oneshot::channel();
    let query = Box::new(move |e: &Engine| {
        let _ = reply.send(e.accounts().get(&TRADER).map(|a| a.balance));
    });
    tx.send(Request::Query(query)).await.unwrap();
    assert_eq!(balance.await.unwrap(), Some(usdc("1")));
    assert_eq!(stored_cursor(&dir).next_block, 2);
    assert_eq!(logs.count("alert="), 0);
    assert!(!run.is_finished(), "a refused stamp must not stop the watcher");
    run.abort();
}
