//! Reorgs and forked answers: the watcher reads only blocks proven to lead to the tagged block,
//! re-proves quietly when blocks above those already read change, and stalls when the chain
//! contradicts blocks it has read.

use alloy::primitives::B256;
use engine_core::command::Command;

use crate::custody::evm::{Beneficiary, VaultDeposit};
use crate::custody::stall::{PollError, Polled, Stall};
use crate::custody::tests::fakes::{
    FakeChain, Logs, Node, TRADER, USDC, canonical_hash, deposit, engine_sequencer, scratch, stored_cursor, usdc,
    watcher, watcher_with_confirmations,
};

#[tokio::test]
async fn a_deposit_reorged_away_before_finality_is_never_credited() {
    let dir = scratch("reorg");
    let (chain, node) = (FakeChain::default(), Node::start(&dir));
    let mut watcher = watcher(&chain, &dir, 1_000, None);
    let first = deposit(0, 7, Beneficiary::Trader(TRADER), 10 * USDC);
    chain.add(first.clone());
    chain.set_final(Some(6));
    assert!(matches!(watcher.poll(&node.tx).await.map(Polled::caught_up), Ok(true)));

    // The reorg drops block 7; the same transaction lands later at another log index.
    chain.remove(&first);
    chain.set_final(Some(10));
    assert!(matches!(watcher.poll(&node.tx).await.map(Polled::caught_up), Ok(true)));
    assert_eq!(node.balance(TRADER).await, None);
    let reincluded =
        VaultDeposit { block: 12, block_hash: canonical_hash(12), log_index: first.log_index + 4, ..first.clone() };
    chain.add(reincluded.clone());
    chain.set_final(Some(12));
    assert!(matches!(watcher.poll(&node.tx).await.map(Polled::caught_up), Ok(true)));

    assert_eq!(node.balance(TRADER).await, Some(usdc("10")));
    let refs = (first.chain_ref(), reincluded.chain_ref());
    assert_eq!(node.read(move |e| (e.is_credited(&refs.0), e.is_credited(&refs.1))).await, (false, true));
}

#[tokio::test]
async fn blocks_that_do_not_lead_to_the_tagged_block_credit_nothing() {
    let dir = scratch("mixed-views");
    let (chain, node) = (FakeChain::default(), Node::start(&dir));
    chain.add(deposit(0, 5, Beneficiary::Trader(TRADER), USDC));
    chain.set_final(Some(5));
    // One backend reports block 5 safe; the backend serving blocks and logs holds another block 5.
    chain.0.lock().unwrap().tag_hash = Some(B256::repeat_byte(0xaa));
    let mut watcher = watcher(&chain, &dir, 1_000, None);

    assert!(matches!(watcher.poll(&node.tx).await, Err(PollError::Stalled(Stall::Unsettled(_)))));
    assert!(chain.reads().is_empty(), "no log is read off an unproven chain");
    assert_eq!(node.balance(TRADER).await, None);

    chain.0.lock().unwrap().tag_hash = None;
    assert!(matches!(watcher.poll(&node.tx).await.map(Polled::caught_up), Ok(true)));
    assert_eq!(node.balance(TRADER).await, Some(usdc("1")));
}

#[tokio::test]
async fn headers_that_do_not_link_credit_nothing() {
    let dir = scratch("broken-link");
    let (chain, node) = (FakeChain::default(), Node::start(&dir));
    chain.add(deposit(0, 3, Beneficiary::Trader(TRADER), USDC));
    chain.set_final(Some(9));
    // The second batch of headers (4..=7) comes from a backend whose block 5 has another parent.
    chain.0.lock().unwrap().broken_link = Some(5);
    let mut watcher = watcher(&chain, &dir, 4, None);

    assert!(matches!(watcher.poll(&node.tx).await, Err(PollError::Stalled(Stall::Unsettled(_)))));
    assert!(chain.reads().is_empty());
    assert_eq!(node.balance(TRADER).await, None);

    chain.0.lock().unwrap().broken_link = None;
    assert!(matches!(watcher.poll(&node.tx).await.map(Polled::caught_up), Ok(false)));
    assert_eq!(node.balance(TRADER).await, Some(usdc("1")));
}

#[tokio::test]
async fn a_deposit_in_a_block_off_the_canonical_chain_pauses_crediting() {
    let dir = scratch("fork");
    let (chain, node) = (FakeChain::default(), Node::start(&dir));
    // A lagging backend still reports block 4 as it was before a reorg.
    let stale =
        VaultDeposit { block_hash: B256::repeat_byte(0xee), ..deposit(0, 4, Beneficiary::Trader(TRADER), USDC) };
    chain.add(stale.clone());
    chain.set_final(Some(6));
    let mut watcher = watcher(&chain, &dir, 1_000, None);

    assert!(matches!(watcher.poll(&node.tx).await, Err(PollError::Stalled(Stall::Unsettled(_)))));
    assert_eq!(node.balance(TRADER).await, None);
    assert!(!dir.join("cursor.json").exists());

    chain.remove(&stale);
    chain.add(deposit(0, 4, Beneficiary::Trader(TRADER), USDC));
    assert!(matches!(watcher.poll(&node.tx).await.map(Polled::caught_up), Ok(true)));
    assert_eq!(node.balance(TRADER).await, Some(usdc("1")));
}

#[tokio::test]
async fn a_reorg_above_the_blocks_read_is_proven_again_without_an_alert() {
    let (logs, _guard) = Logs::capture();
    let dir = scratch("reorg-above");
    let (chain, node) = (FakeChain::default(), Node::start(&dir));
    chain.add(deposit(0, 9, Beneficiary::Trader(TRADER), USDC));
    // The tag at 10 with two confirmations makes block 8 final; blocks 9 and 10 are proven but
    // not yet read.
    chain.set_final(Some(10));
    let mut watcher = watcher_with_confirmations(&chain, &dir, 1_000, None, 2);
    assert!(matches!(watcher.poll(&node.tx).await.map(Polled::caught_up), Ok(true)));
    assert_eq!(node.balance(TRADER).await, None);

    // Blocks 9 and 10 are replaced; the deposit lands again in the new block 9.
    {
        let mut state = chain.0.lock().unwrap();
        state.forks.insert(9, B256::repeat_byte(0x99));
        state.forks.insert(10, B256::repeat_byte(0x10));
        let moved = VaultDeposit { block_hash: B256::repeat_byte(0x99), ..state.deposits[0].clone() };
        state.deposits = vec![moved];
        state.final_block = Some(12);
    }
    assert!(matches!(watcher.poll(&node.tx).await.map(Polled::caught_up), Ok(true)));

    assert_eq!(node.balance(TRADER).await, Some(usdc("1")));
    assert_eq!(logs.count("alert="), 0, "a reorg the finality policy allows raises nothing");
}

#[tokio::test]
async fn a_reorg_that_rewrites_a_block_already_read_stalls() {
    let dir = scratch("reorg-below");
    let (chain, node) = (FakeChain::default(), Node::start(&dir));
    chain.set_final(Some(10));
    let mut watcher = watcher(&chain, &dir, 1_000, None);
    assert!(matches!(watcher.poll(&node.tx).await.map(Polled::caught_up), Ok(true)));

    // Block 10, already read, is replaced: the chain no longer continues from it.
    chain.0.lock().unwrap().forks.insert(10, B256::repeat_byte(0x10));
    chain.set_final(Some(12));

    assert!(matches!(watcher.poll(&node.tx).await, Err(PollError::Stalled(Stall::Unsettled(_)))));
    assert_eq!(chain.reads(), vec![(0, 10)]);
}

#[tokio::test]
async fn a_restart_after_a_block_already_read_changed_stalls() {
    let dir = scratch("restart-reorg");
    let (chain, node) = (FakeChain::default(), Node::start(&dir));
    chain.add(deposit(0, 10, Beneficiary::Trader(TRADER), USDC));
    chain.set_final(Some(10));
    assert!(matches!(watcher(&chain, &dir, 1_000, None).poll(&node.tx).await.map(Polled::caught_up), Ok(true)));

    // While the watcher is down, block 10 (already read, its deposit credited) is replaced.
    chain.0.lock().unwrap().forks.insert(10, B256::repeat_byte(0x10));
    chain.set_final(Some(12));
    let mut restarted = watcher(&chain, &dir, 1_000, node.read(|e| e.last_seq()).await);

    assert!(matches!(restarted.poll(&node.tx).await, Err(PollError::Stalled(Stall::Unsettled(_)))));
    assert_eq!(chain.reads(), vec![(0, 10)]);
}

#[tokio::test]
async fn a_restart_part_way_through_crediting_proves_the_chain_through_that_range() {
    let dir = scratch("crediting-restart");
    let chain = FakeChain::default();
    let first = deposit(0, 5, Beneficiary::Trader(TRADER), USDC);
    chain.add(first.clone());
    chain.set_final(Some(5));
    // The node stops after crediting block 5 but before the cursor moves past it.
    let stopped = watcher(&chain, &dir, 1_000, None).poll(&engine_sequencer(false)).await;
    assert!(matches!(stopped, Err(PollError::Fatal(_))));

    // While it is down, block 5 is replaced and the transaction lands in block 7 at another index.
    {
        let mut state = chain.0.lock().unwrap();
        state.forks.insert(5, B256::repeat_byte(0x55));
        state.deposits = vec![VaultDeposit { block: 7, block_hash: canonical_hash(7), log_index: 9, ..first }];
        state.final_block = Some(8);
    }
    let restarted = watcher(&chain, &dir, 1_000, Some(0)).poll(&engine_sequencer(true)).await;

    assert!(matches!(restarted, Err(PollError::Stalled(Stall::Unsettled(_)))));
    assert_eq!(chain.reads(), vec![(0, 5)], "the moved deposit is never read");
}

#[tokio::test]
async fn a_forked_answer_to_a_window_read_credits_nothing() {
    let dir = scratch("forked-window");
    let (chain, node) = (FakeChain::default(), Node::start(&dir));
    chain.set_final(Some(9));
    let mut watcher = watcher(&chain, &dir, 4, None);
    assert!(matches!(watcher.poll(&node.tx).await.map(Polled::caught_up), Ok(false)));

    // The next window's headers, logs, and count come from a backend on another fork: blocks
    // 4..=7 link among themselves but not to the proven block 7, and one holds a deposit.
    for number in 4..=7u8 {
        chain.0.lock().unwrap().forks.insert(u64::from(number), B256::repeat_byte(number));
    }
    chain.add(VaultDeposit { block_hash: B256::repeat_byte(5), ..deposit(0, 5, Beneficiary::Trader(TRADER), USDC) });

    // The tag is unchanged, so later polls reuse the proof; never the forked headers.
    for _ in 0..3 {
        assert!(matches!(watcher.poll(&node.tx).await, Err(PollError::Stalled(Stall::Unsettled(_)))));
    }
    assert_eq!(chain.reads(), vec![(0, 3)]);
    assert_eq!(node.balance(TRADER).await, None);
}

#[tokio::test]
async fn a_shorter_range_read_after_a_restart_keeps_the_range_being_credited() {
    let dir = scratch("crediting-short");
    let chain = FakeChain::default();
    chain.add(deposit(0, 2, Beneficiary::Trader(TRADER), USDC));
    let first = deposit(1, 8, Beneficiary::Trader(TRADER), USDC);
    chain.add(first.clone());
    chain.set_final(Some(9));
    // Stopped after crediting blocks 2 and 8, with blocks 0..=9 marked as being credited.
    let stopped = watcher(&chain, &dir, 10, None).poll(&engine_sequencer(false)).await;
    assert!(matches!(stopped, Err(PollError::Fatal(_))));

    // After the restart a capped log read halves the range, and blocks 0..=4, with the deposit
    // already credited in block 2, are read alone.
    chain.0.lock().unwrap().log_range_cap = Some(5);
    let tx = engine_sequencer(true);
    let mut restarted = watcher(&chain, &dir, 10, Some(0));
    assert!(matches!(restarted.poll(&tx).await, Err(PollError::Stalled(Stall::Rpc(_)))));
    assert!(matches!(restarted.poll(&tx).await.map(Polled::caught_up), Ok(false)));
    assert_eq!(stored_cursor(&dir).crediting.map(|(number, _)| number), Some(9));

    // Blocks 5..=9 are then replaced and the transaction lands in block 12 at another index.
    {
        let mut state = chain.0.lock().unwrap();
        for number in 5..=9 {
            state.forks.insert(number, B256::repeat_byte(number as u8));
        }
        state.deposits.retain(|d| d.id == 0);
        state.deposits.push(VaultDeposit { block: 12, block_hash: canonical_hash(12), log_index: 9, ..first });
        state.final_block = Some(13);
    }
    assert!(matches!(restarted.poll(&tx).await, Err(PollError::Stalled(Stall::Unsettled(_)))));
}

#[tokio::test]
async fn a_rescan_that_finds_a_credited_transaction_moved_stalls() {
    let dir = scratch("moved-credit");
    let (chain, node) = (FakeChain::default(), Node::start(&dir));
    let first = deposit(0, 5, Beneficiary::Trader(TRADER), USDC);
    chain.add(first.clone());
    chain.set_final(Some(5));
    assert!(matches!(watcher(&chain, &dir, 1_000, None).poll(&node.tx).await.map(Polled::caught_up), Ok(true)));

    // The cursor is lost, and a reorg below finality moves the transaction to another index.
    std::fs::remove_file(dir.join("cursor.json")).unwrap();
    {
        let mut state = chain.0.lock().unwrap();
        state.forks.insert(5, B256::repeat_byte(0x55));
        state.deposits = vec![VaultDeposit { block: 7, block_hash: canonical_hash(7), log_index: 9, ..first }];
        state.final_block = Some(8);
    }
    let mut rescan = watcher(&chain, &dir, 1_000, node.last_seq().await);

    assert!(matches!(rescan.poll(&node.tx).await, Err(PollError::Stalled(Stall::Inconsistent(_)))));
    assert_eq!(node.balance(TRADER).await, Some(usdc("1")), "credited once");
}

#[tokio::test]
async fn a_transaction_credited_in_part_has_the_rest_credited() {
    let dir = scratch("partial-credit");
    let (chain, node) = (FakeChain::default(), Node::start(&dir));
    // One transaction makes two deposits; a crash left only the first credited.
    let first = deposit(0, 5, Beneficiary::Trader(TRADER), USDC);
    let second = VaultDeposit { id: 1, log_index: 2, ..first.clone() };
    chain.add(first.clone());
    chain.add(second);
    chain.set_final(Some(5));
    let account = TRADER;
    let amount = usdc("1");
    node.submit(Command::Deposit { account, amount, chain_ref: first.chain_ref() }).await;

    let polled = watcher(&chain, &dir, 1_000, node.last_seq().await).poll(&node.tx).await;
    assert!(matches!(polled.map(Polled::caught_up), Ok(true)));
    assert_eq!(node.balance(TRADER).await, Some(usdc("2")));
}
