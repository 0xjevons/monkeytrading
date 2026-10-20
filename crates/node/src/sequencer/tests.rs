use super::*;
use std::fs;
use std::path::PathBuf;

use crate::log::{Manifest, SegmentedLog, segment_seq};
use crate::test_support::{
    SEGMENT_COMMANDS, TRADER, TestSequencer, deposit, dev_config, log_messages, scratch_dir, sequencer_config, withdraw,
};

const V1_DEV: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/v1-dev.jsonl");

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
    // The old node signed that withdrawal itself; recovery must not sign it again.
    assert!(replayed.unsigned.is_empty());
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
fn custody_gets_a_withdrawal_when_its_batch_is_flushed_not_when_it_is_signed() {
    let dir = scratch_dir("seq-custody");
    let (stream, _) = broadcast::channel(16);
    let (withdrawals_tx, mut withdrawals) = mpsc::unbounded_channel();
    let key = Arc::new(EnclaveKey::generate());
    let manifest = watch::channel(Manifest::default()).0;
    let mut seq = Sequencer::recover(sequencer_config(&dir, manifest), key, stream, withdrawals_tx).unwrap();
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

#[tokio::test]
async fn replay_refuses_an_approved_withdrawal_whose_signed_entry_is_missing() {
    let dir = scratch_dir("seq-missing-withdrawal");
    let node = TestSequencer::start(&dir, 16);
    node.submit(deposit(100, "dep-1")).await;
    node.submit(withdraw(40)).await;
    node.submit(Command::Tick).await;
    node.stop().await;
    let mut entries = SegmentedLog::open(&dir).unwrap().entries;
    let at = entries.iter().position(|e| matches!(e, LogEntry::Withdrawal(_))).unwrap();
    entries.remove(at);
    let err = replay(&entries).err().unwrap();
    assert!(format!("{err:#}").contains("has no signed entry"), "{err:#}");
}

#[tokio::test]
async fn a_restart_signs_a_withdrawal_the_log_ends_before_and_hands_it_to_custody() {
    let dir = scratch_dir("seq-resign");
    let mut node = TestSequencer::start(&dir, 16);
    node.submit(deposit(100, "dep-1")).await;
    node.submit(withdraw(40)).await;
    assert_eq!(node.withdrawals.try_recv().unwrap().nonce, 1);
    node.stop().await;
    // As if the process died after the withdraw command's line reached disk and before the rest.
    let segment = last_segment(&dir);
    let text = fs::read_to_string(&segment).unwrap();
    let command = text.find(r#""cmd":{"type":"withdraw""#).unwrap();
    fs::write(&segment, &text[..command + text[command..].find('\n').unwrap() + 1]).unwrap();
    let approved = (TRADER, Fixed::from_int(40), 1);
    assert_eq!(replay(&SegmentedLog::open(&dir).unwrap().entries).unwrap().unsigned, [approved]);

    let mut node = TestSequencer::start(&dir, 16);
    let signed = node.withdrawals.try_recv().unwrap();
    assert_eq!((signed.account, signed.amount, signed.nonce), approved);
    node.stop().await;
    assert!(replay(&SegmentedLog::open(&dir).unwrap().entries).unwrap().unsigned.is_empty());
}

#[tokio::test]
async fn replay_refuses_a_command_without_logged_events_after_ones_with_them() {
    let dir = scratch_dir("seq-events-cutover");
    let node = TestSequencer::start(&dir, 16);
    node.submit(deposit(100, "dep-1")).await;
    node.stop().await;
    let mut entries = SegmentedLog::open(&dir).unwrap().entries;
    let last_command = entries.iter().rposition(|e| matches!(e, LogEntry::Command(_))).unwrap();
    if let LogEntry::Command(c) = &mut entries[last_command] {
        c.events = None;
    }
    let err = replay(&entries).err().unwrap();
    assert!(format!("{err:#}").contains("has no logged events, though earlier commands do"), "{err:#}");
}

#[tokio::test]
async fn replay_refuses_a_checkpoint_after_an_approval_whose_signed_entry_is_missing() {
    let dir = scratch_dir("seq-missing-withdrawal-checkpoint");
    let node = TestSequencer::start(&dir, 16);
    node.submit(deposit(100, "dep-1")).await;
    node.tick_to_checkpoint().await;
    node.submit(withdraw(40)).await;
    node.stop().await;
    let mut entries = SegmentedLog::open(&dir).unwrap().entries;
    assert!(matches!(entries.last(), Some(LogEntry::Checkpoint(_))));
    let at = entries.iter().position(|e| matches!(e, LogEntry::Withdrawal(_))).unwrap();
    entries.remove(at);
    let err = replay(&entries).err().unwrap();
    assert!(format!("{err:#}").contains("has no signed entry"), "{err:#}");
}

#[test]
fn a_fresh_log_records_client_auth_and_activates_the_latest_version_before_genesis() {
    let dir = scratch_dir("activation-fresh");
    let mut seq = recover(&dir, &signed(31_337));
    seq.activate_protocol().unwrap();
    seq.genesis(&dev_config().genesis).unwrap();
    let entries = entries(&dir);
    let cmds = commands(&entries);
    assert_eq!(cmds[0], Command::Admin(AdminCommand::SetClientAuth(signed(31_337))));
    assert_eq!(cmds[1], Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::LATEST }));
    let replayed = replay(&entries).unwrap();
    assert_eq!(replayed.engine.protocol_version(), ProtocolVersion::LATEST);
    assert_eq!(replayed.engine.client_auth(), Some(&signed(31_337)));
}

#[test]
fn an_existing_v1_log_activates_at_its_next_seq_and_still_verifies() {
    let dir = scratch_dir("activation-v1");
    fs::create_dir_all(&dir).unwrap();
    // A log written as one file before segments existed continues as the first segment.
    fs::copy(V1_DEV, dir.join("00000000000000000000.jsonl")).unwrap();
    recover(&dir, &signed(31_337)).activate_protocol().unwrap();
    // The original 63 commands and 3 checkpoints, then the activation and its checkpoint.
    let replayed = replay(&entries(&dir)).unwrap();
    assert_eq!((replayed.commands, replayed.checkpoints), (65, 4));
    assert_eq!(replayed.activations, [(ProtocolVersion::LATEST, 64)]);
}

#[test]
fn a_restart_with_the_recorded_client_auth_sequences_nothing() {
    let dir = scratch_dir("activation-restart");
    recover(&dir, &signed(31_337)).activate_protocol().unwrap();
    let written = entries(&dir).len();
    recover(&dir, &signed(31_337)).activate_protocol().unwrap();
    assert_eq!(entries(&dir).len(), written);
}

#[test]
fn a_genesis_cut_short_runs_again_on_restart() {
    // With house capital and without it, a genesis cut short at any command runs again in full, and
    // a complete one is not run again.
    for capital in [dev_config().genesis.house_capital, Fixed::ZERO] {
        let mut genesis = dev_config().genesis;
        genesis.house_capital = capital;
        let all = genesis_commands(&genesis);
        for cut in [0, 1, all.len() / 2, all.len() - 1] {
            let name = format!("capital-{capital}-cut-{cut}");
            let dir = scratch_dir(&format!("genesis-{name}"));
            let mut seq = recover(&dir, &signed(31_337));
            seq.activate_protocol().unwrap();
            seq.sequence_required(all[..cut].to_vec()).unwrap();
            drop(seq);

            let mut seq = recover(&dir, &signed(31_337));
            assert!(seq.needs_genesis(&genesis), "{name}");
            seq.activate_protocol().unwrap();
            seq.genesis(&genesis).unwrap();
            drop(seq);
            let replayed = replay(&entries(&dir)).unwrap();
            assert_eq!(replayed.engine.markets().len(), genesis.markets.len(), "{name}");
            assert_eq!(replayed.engine.house_cash(), genesis.house_capital, "{name}");
            assert!(!recover(&dir, &signed(31_337)).needs_genesis(&genesis), "{name}");
        }
    }
}

#[test]
fn a_log_holding_only_client_auth_activates_on_restart() {
    let dir = scratch_dir("activation-auth-only");
    let auth = Command::Admin(AdminCommand::SetClientAuth(signed(31_337)));
    recover(&dir, &signed(31_337)).sequence_required(vec![auth.clone()]).unwrap();
    recover(&dir, &signed(31_337)).activate_protocol().unwrap();
    let written = entries(&dir);
    assert_eq!(commands(&written).iter().filter(|cmd| **cmd == auth).count(), 1);
    assert_eq!(replay(&written).unwrap().engine.protocol_version(), ProtocolVersion::LATEST);
    recover(&dir, &signed(31_337)).activate_protocol().unwrap();
    assert_eq!(entries(&dir).len(), written.len());
}

#[test]
fn a_log_recording_other_client_auth_refuses_to_start_and_writes_nothing() {
    let dir = scratch_dir("activation-mismatch");
    recover(&dir, &signed(31_337)).activate_protocol().unwrap();
    let written = entries(&dir).len();
    for other in [ClientAuth::Unsigned, signed(1)] {
        let err = try_recover(&dir, &other).0.err().unwrap();
        assert!(format!("{err:#}").contains("the log records client auth"), "{err:#}");
    }
    assert_eq!(entries(&dir).len(), written);
}

#[test]
fn a_restart_under_other_client_auth_signs_no_withdrawal_the_log_ends_before() {
    let dir = scratch_dir("activation-mismatch-withdrawal");
    let mut seq = recover(&dir, &ClientAuth::Unsigned);
    seq.activate_protocol().unwrap();
    seq.genesis(&dev_config().genesis).unwrap();
    seq.sequence(deposit(100, "dep-1")).unwrap();
    seq.sequence(withdraw(40)).unwrap();
    seq.flush().unwrap();
    drop(seq);
    // As if the process died after the withdraw command's line reached disk and before the rest.
    let segment = last_segment(&dir);
    let text = fs::read_to_string(&segment).unwrap();
    let command = text.find(r#""cmd":{"type":"withdraw""#).unwrap();
    let cut = &text[..command + text[command..].find('\n').unwrap() + 1];
    fs::write(&segment, cut).unwrap();

    let (recovered, mut withdrawals) = try_recover(&dir, &signed(31_337));
    let err = recovered.err().unwrap();
    assert!(format!("{err:#}").contains("the log records client auth"), "{err:#}");
    assert!(withdrawals.try_recv().is_err(), "a withdrawal was signed under the other chain and vault");
    assert_eq!(fs::read_to_string(&segment).unwrap(), cut);
}

#[test]
fn genesis_credits_house_capital_only_when_there_is_some() {
    let mut genesis = crate::test_support::dev_config().genesis;
    let credit = |genesis: &Genesis| {
        genesis_commands(genesis).into_iter().filter(|cmd| matches!(cmd, Command::HouseCapital { .. })).count()
    };
    assert_eq!(credit(&genesis), 1);
    genesis.house_capital = Fixed::ZERO;
    assert_eq!(credit(&genesis), 0);
    // Every market is still listed.
    let listings = genesis_commands(&genesis)
        .iter()
        .filter(|cmd| matches!(cmd, Command::Admin(AdminCommand::UpsertMarket(_))))
        .count();
    assert_eq!(listings, genesis.markets.len());
}

// ── Helpers ──

/// The segment file the log appends to.
fn last_segment(dir: &Path) -> PathBuf {
    let mut names: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|name| segment_seq(name).is_some())
        .collect();
    names.sort();
    dir.join(names.last().unwrap())
}

/// Signed requests under the vault `0xaa…` on `chain_id`.
fn signed(chain_id: u64) -> ClientAuth {
    ClientAuth::Eip712 { chain_id, vault: AccountId([0xaa; 20]) }
}

/// A sequencer recovered from the log in `dir` under `client_auth`, with a fresh enclave key.
fn recover(dir: &Path, client_auth: &ClientAuth) -> Sequencer {
    try_recover(dir, client_auth).0.unwrap()
}

/// `recover`'s result, and what custody would receive from it.
fn try_recover(
    dir: &Path,
    client_auth: &ClientAuth,
) -> (anyhow::Result<Sequencer>, mpsc::UnboundedReceiver<SignedWithdrawal>) {
    let (stream, _) = broadcast::channel(16);
    let (withdrawals_tx, withdrawals) = mpsc::unbounded_channel();
    let (manifest, _) = watch::channel(Manifest::default());
    let cfg = SequencerConfig { client_auth: client_auth.clone(), ..sequencer_config(dir, manifest) };
    (Sequencer::recover(cfg, Arc::new(EnclaveKey::generate()), stream, withdrawals_tx), withdrawals)
}

/// Every entry of the log in `dir`.
fn entries(dir: &Path) -> Vec<LogEntry> {
    SegmentedLog::open(dir).unwrap().entries
}

/// The commands among `entries`, in order.
fn commands(entries: &[LogEntry]) -> Vec<Command> {
    entries
        .iter()
        .filter_map(|e| match e {
            LogEntry::Command(c) => Some(c.sequenced.cmd.clone()),
            _ => None,
        })
        .collect()
}
