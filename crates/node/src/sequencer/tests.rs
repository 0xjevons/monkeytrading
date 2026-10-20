use super::*;

use crate::log::Manifest;
use crate::test_support::{SEGMENT_COMMANDS, TestSequencer, log_messages, scratch_dir, sequencer_config};

fn tick(seq: u64) -> LogEntry {
    LogEntry::Command(CommandEntry { sequenced: Sequenced { seq, ts: 0, cmd: Command::Tick }, events: None })
}

fn checkpoint(seq: u64, root: String) -> LogEntry {
    LogEntry::Checkpoint(Checkpoint { seq, root, signature: String::new(), signer: String::new() })
}

#[test]
fn replay_verifies_checkpoint_roots() {
    let mut engine = Engine::new();
    engine.apply(&Sequenced { seq: 0, ts: 0, cmd: Command::Tick }).unwrap();
    let root = format!("0x{}", hex::encode(engine.state_root()));
    let replayed = replay(&[tick(0), checkpoint(0, root)]).unwrap();
    assert_eq!((replayed.commands, replayed.checkpoints), (1, 1));
}

#[test]
fn replay_refuses_a_tampered_root_or_a_misplaced_checkpoint() {
    let err = replay(&[tick(0), checkpoint(0, "0xbad".into())]).err().unwrap();
    assert!(format!("{err:#}").contains("state root mismatch"), "{err:#}");
    let err = replay(&[tick(0), checkpoint(5, "0x".into())]).err().unwrap();
    assert!(format!("{err:#}").contains("checkpoint for seq 5"), "{err:#}");
}

#[test]
fn replay_refuses_a_gap_in_the_log() {
    let err = replay(&[tick(0), tick(2)]).err().unwrap();
    assert!(format!("{err:#}").contains("applying seq 2"), "{err:#}");
}

#[test]
fn replay_accepts_a_log_written_before_events_and_withdrawals_were_logged() {
    let mut engine = Engine::new();
    let commands = [deposit(100, "dep-1"), withdraw(40)];
    let mut entries: Vec<_> = commands
        .into_iter()
        .enumerate()
        .map(|(seq, cmd)| {
            let sequenced = Sequenced { seq: seq as u64, ts: 0, cmd };
            engine.apply(&sequenced).unwrap();
            LogEntry::Command(CommandEntry { sequenced, events: None })
        })
        .collect();
    entries.push(checkpoint(1, format!("0x{}", hex::encode(engine.state_root()))));
    let replayed = replay(&entries).unwrap();
    assert_eq!((replayed.commands, replayed.checkpoints), (2, 1));
}

#[tokio::test]
async fn one_withdrawal_puts_command_withdrawal_and_checkpoint_at_one_seq() {
    let dir = scratch_dir("seq-offsets");
    let node = TestSequencer::start(&dir, 1024);
    let mut live = node.stream.subscribe();
    node.submit(deposit(100, "dep-1")).await;
    let seq = node.tick_to_checkpoint().await;
    node.submit(withdraw(40)).await;

    let at_seq: Vec<_> = std::iter::from_fn(|| live.try_recv().ok()).filter(|m| m.offset.seq == seq).collect();
    let kinds: Vec<_> = at_seq
        .iter()
        .map(|m| match &m.body {
            StreamBody::Command(_) => (m.offset.index, "command"),
            StreamBody::Withdrawal(_) => (m.offset.index, "withdrawal"),
            StreamBody::Checkpoint(_) => (m.offset.index, "checkpoint"),
        })
        .collect();
    assert_eq!(kinds, [(0, "command"), (1, "withdrawal"), (2, "checkpoint")]);
    // The stream and the log carry the same messages at the same offsets.
    let published: Vec<_> = at_seq.iter().map(|m| serde_json::to_string(&**m).unwrap()).collect();
    let logged = log_messages(&dir, Offset::start_of(0));
    let start = logged.iter().position(|m| *m == published[0]).expect("command on disk");
    assert_eq!(logged[start..start + 3], published);
}

#[test]
fn custody_gets_a_withdrawal_only_once_its_log_entry_is_flushed() {
    let dir = scratch_dir("seq-custody");
    let (stream, _) = broadcast::channel(16);
    let (withdrawals_tx, mut withdrawals) = mpsc::unbounded_channel();
    let key = Arc::new(EnclaveKey::generate());
    let mut seq = Sequencer::recover(sequencer_config(&dir), key, stream, withdrawals_tx).unwrap();
    seq.sequence(deposit(100, "dep-1")).unwrap();
    seq.sequence(withdraw(40)).unwrap();
    assert!(withdrawals.try_recv().is_err());
    seq.flush().unwrap();
    assert_eq!(withdrawals.try_recv().unwrap().nonce, 1);
}

#[tokio::test]
async fn replay_refuses_logged_events_or_withdrawals_that_replay_does_not_produce() {
    let dir = scratch_dir("seq-tamper");
    let node = TestSequencer::start(&dir, 16);
    node.submit(deposit(100, "dep-1")).await;
    node.submit(withdraw(40)).await;
    node.stop().await;
    let entries = SegmentedLog::open(&dir).unwrap().entries;
    replay(&entries).unwrap();

    let mut altered = entries.clone();
    let deposit_at = altered
        .iter()
        .position(|e| matches!(e, LogEntry::Command(c) if matches!(c.sequenced.cmd, Command::Deposit { .. })))
        .unwrap();
    if let LogEntry::Command(c) = &mut altered[deposit_at] {
        c.events = Some(Vec::new());
    }
    let err = replay(&altered).err().unwrap();
    assert!(format!("{err:#}").contains("events logged at seq"), "{err:#}");

    let withdrawal_at = entries.iter().position(|e| matches!(e, LogEntry::Withdrawal(_))).unwrap();
    let mut doubled = entries.clone();
    doubled.insert(withdrawal_at, entries[withdrawal_at].clone());
    let err = replay(&doubled).err().unwrap();
    assert!(format!("{err:#}").contains("was not approved"), "{err:#}");

    let mut forged = entries.clone();
    if let LogEntry::Withdrawal(w) = &mut forged[withdrawal_at] {
        w.amount = Fixed::from_int(41);
    }
    let err = replay(&forged).err().unwrap();
    assert!(format!("{err:#}").contains("was not approved"), "{err:#}");
}

#[tokio::test]
async fn a_restarted_sequencer_continues_the_log_offsets_and_segments() {
    let dir = scratch_dir("seq-restart");
    let node = TestSequencer::start(&dir, 16);
    node.submit(deposit(100, "dep-1")).await;
    node.stop().await;
    let before = replay(&SegmentedLog::open(&dir).unwrap().entries).unwrap().engine.last_seq().unwrap();

    let node = TestSequencer::start(&dir, 16);
    let mut live = node.stream.subscribe();
    for n in 0..SEGMENT_COMMANDS {
        node.submit(deposit(1, &format!("dep-after-{n}"))).await;
    }
    assert_eq!(live.try_recv().unwrap().offset, Offset::start_of(before + 1));
    node.stop().await;

    let log = SegmentedLog::open(&dir).unwrap();
    let replayed = replay(&log.entries).unwrap();
    assert_eq!(replayed.engine.last_seq(), Some(before + SEGMENT_COMMANDS));
    assert!(Manifest::load(&dir).unwrap().segments.len() >= 2);
}

// ── Helpers ──

fn deposit(amount: i64, chain_ref: &str) -> Command {
    crate::test_support::deposit(amount, chain_ref)
}

fn withdraw(amount: i64) -> Command {
    crate::test_support::withdraw(amount)
}
