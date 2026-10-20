use super::*;
use std::fs;
use std::io::Write as _;

use engine_core::Fixed;
use engine_core::command::Command;
use engine_core::event::{CancelReason, Fill, Reject};
use engine_core::price::PriceError;
use engine_core::types::{AccountId, Side};
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use crate::test_support::scratch_dir;

const SEG_0: &str = "00000000000000000000.jsonl";
const SEG_2: &str = "00000000000000000002.jsonl";
const SEG_4: &str = "00000000000000000004.jsonl";

#[test]
fn offsets_read_and_write_as_seq_colon_index_and_order_by_seq_then_index() {
    let offset: Offset = "12:3".parse().unwrap();
    assert_eq!(offset, Offset { seq: 12, index: 3 });
    assert_eq!(offset.to_string(), "12:3");
    assert_eq!(serde_json::to_string(&offset).unwrap(), "\"12:3\"");
    assert_eq!(serde_json::from_str::<Offset>("\"12:3\"").unwrap(), offset);
    for bad in ["12", "12:", ":3", "a:3", "12:-1", "12:3:4"] {
        assert!(bad.parse::<Offset>().is_err(), "{bad}");
    }
    assert!(Offset { seq: 1, index: 9 } < Offset { seq: 2, index: 0 });
    assert!(Offset { seq: 2, index: 0 } < Offset { seq: 2, index: 1 });
    assert_eq!(offset.next(), Offset { seq: 12, index: 4 });
}

#[test]
fn a_command_entry_from_before_events_were_logged_still_parses() {
    let line = r#"{"entry":"command","seq":0,"ts":5,"cmd":{"type":"tick"}}"#;
    let entry: LogEntry = serde_json::from_str(line).unwrap();
    let sequenced = Sequenced { seq: 0, ts: 5, cmd: Command::Tick };
    assert_eq!(entry, LogEntry::Command(CommandEntry { sequenced, events: None }));
    assert_eq!(serde_json::to_string(&entry).unwrap(), line);
}

#[test]
fn a_command_entry_carries_its_events_through_the_file_format() {
    let events = vec![Event::Deposited { account: AccountId([1; 20]), amount: Fixed::from_int(5) }];
    let entry = LogEntry::Command(CommandEntry {
        sequenced: Sequenced { seq: 1, ts: 5, cmd: Command::Tick },
        events: Some(events),
    });
    let line = serde_json::to_string(&entry).unwrap();
    assert!(line.contains(r#""events":[{"type":"deposited""#), "{line}");
    assert_eq!(serde_json::from_str::<LogEntry>(&line).unwrap(), entry);
}

#[test]
fn every_event_and_rejection_reason_survives_the_file_format() {
    let entry = LogEntry::Command(CommandEntry {
        sequenced: Sequenced { seq: 1, ts: 5, cmd: Command::Tick },
        events: Some(every_event()),
    });
    let line = serde_json::to_string(&entry).unwrap();
    assert_eq!(serde_json::from_str::<LogEntry>(&line).unwrap(), entry, "{line}");
}

#[test]
fn offsets_number_each_seqs_entries_from_its_command() {
    let entries = [cmd(0), cmd(1), withdrawal(1), checkpoint(1), cmd(2)];
    let offsets: Vec<_> = entries
        .iter()
        .scan(None, |prev, entry| {
            *prev = Some(offset_of(*prev, entry).unwrap());
            *prev
        })
        .map(|o| (o.seq, o.index))
        .collect();
    assert_eq!(offsets, [(0, 0), (1, 0), (1, 1), (1, 2), (2, 0)]);
    assert!(offset_of(None, &checkpoint(0)).is_err());
    assert!(offset_of(None, &withdrawal(1)).is_err());
}

#[test]
fn a_missing_log_is_empty_and_a_corrupt_line_is_an_error() {
    let path = scratch_dir("corrupt").join("log.jsonl");
    assert!(read_all(&path).unwrap().is_empty());
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "\n{\"entry\":\"command\",\"seq\":0,\"ts\":0,\"cmd\":{\"type\":\"tick\"}}\nnot json\n").unwrap();
    let err = read_all(&path).unwrap_err();
    assert!(format!("{err:#}").contains("log line 3"), "{err:#}");
}

#[test]
fn segments_start_at_a_command_and_the_manifest_lists_completed_ones() {
    let dir = scratch_dir("rotate");
    let entries = [cmd(0), cmd(1), withdrawal(1), checkpoint(1), cmd(2), cmd(3), cmd(4)];
    write(&dir, 2, &entries);

    assert_eq!(files(&dir), [SEG_0, SEG_2, SEG_4, MANIFEST]);
    // Seq 1's withdrawal and checkpoint stay with its command.
    assert_eq!(read_all(&dir.join(SEG_0)).unwrap(), entries[..4]);
    let manifest = Manifest::load(&dir).unwrap();
    let ranges: Vec<_> = manifest.segments.iter().map(|s| (s.file.as_str(), s.first_seq, s.last_seq)).collect();
    assert_eq!(ranges, [(SEG_0, 0, 1), (SEG_2, 2, 3)]);
    for listed in &manifest.segments {
        let bytes = fs::read(dir.join(&listed.file)).unwrap();
        assert_eq!(listed.bytes, bytes.len() as u64);
        assert_eq!(listed.sha256, hex::encode(Sha256::digest(&bytes)));
    }
    assert_eq!(SegmentedLog::open(&dir).unwrap().entries, entries);
}

#[test]
fn a_reopened_log_lists_its_full_last_segment_at_the_next_command() {
    // Also what a crash between syncing a full segment and storing the manifest leaves.
    let dir = scratch_dir("reopen-full");
    write(&dir, 2, &[cmd(0), cmd(1), cmd(2), cmd(3)]);
    assert_eq!(Manifest::load(&dir).unwrap().segments.len(), 1);

    write(&dir, 2, &[cmd(4)]);
    let ranges: Vec<_> = Manifest::load(&dir).unwrap().segments.iter().map(|s| (s.first_seq, s.last_seq)).collect();
    assert_eq!(ranges, [(0, 1), (2, 3)]);
    assert_eq!(files(&dir), [SEG_0, SEG_2, SEG_4, MANIFEST]);
    assert_eq!(SegmentedLog::open(&dir).unwrap().entries, [cmd(0), cmd(1), cmd(2), cmd(3), cmd(4)]);
}

#[test]
fn a_reopened_log_appends_to_a_last_segment_with_room() {
    let dir = scratch_dir("reopen-room");
    write(&dir, 2, &[cmd(0), cmd(1), cmd(2)]);
    write(&dir, 2, &[cmd(3)]);
    assert_eq!(files(&dir), [SEG_0, SEG_2, MANIFEST]);
    assert_eq!(read_all(&dir.join(SEG_2)).unwrap(), [cmd(2), cmd(3)]);
}

#[test]
fn a_crash_after_listing_a_segment_but_before_starting_the_next_resumes_cleanly() {
    let dir = scratch_dir("crash-listed");
    write(&dir, 2, &[cmd(0), cmd(1), cmd(2), cmd(3), cmd(4)]);
    // As if the writer stopped right after listing segment 2.
    fs::remove_file(dir.join(SEG_4)).unwrap();

    write(&dir, 2, &[cmd(4), cmd(5)]);
    assert_eq!(files(&dir), [SEG_0, SEG_2, SEG_4, MANIFEST]);
    assert_eq!(read_all(&dir.join(SEG_4)).unwrap(), [cmd(4), cmd(5)]);
    assert_eq!(Manifest::load(&dir).unwrap().segments.len(), 2);
}

#[test]
fn a_crash_after_creating_a_segment_but_before_writing_it_resumes_cleanly() {
    let dir = scratch_dir("crash-empty");
    write(&dir, 2, &[cmd(0), cmd(1)]);
    write(&dir, 2, &[cmd(2)]);
    fs::write(dir.join(SEG_2), "").unwrap();

    write(&dir, 2, &[cmd(2)]);
    assert_eq!(SegmentedLog::open(&dir).unwrap().entries, [cmd(0), cmd(1), cmd(2)]);
    // An empty last segment named for any other seq is refused.
    fs::rename(dir.join(SEG_2), dir.join(SEG_4)).unwrap();
    fs::write(dir.join(SEG_4), "").unwrap();
    let err = SegmentedLog::open(&dir).err().unwrap();
    assert!(format!("{err:#}").contains("not named for the next seq 2"), "{err:#}");
}

#[test]
fn opening_refuses_a_segment_that_differs_from_its_manifest_entry() {
    let dir = scratch_dir("tampered");
    write(&dir, 2, &[cmd(0), cmd(1), cmd(2)]);
    let path = dir.join(SEG_0);
    let text = fs::read_to_string(&path).unwrap();
    fs::write(&path, text.replacen("\"ts\":0,", "\"ts\":9,", 1)).unwrap();
    let err = SegmentedLog::open(&dir).err().unwrap();
    assert!(format!("{err:#}").contains(&format!("does not match {MANIFEST}")), "{err:#}");

    fs::remove_file(&path).unwrap();
    let err = SegmentedLog::open(&dir).err().unwrap();
    assert!(format!("{err:#}").contains(&format!("does not match {MANIFEST}")), "{err:#}");
}

#[test]
fn opening_refuses_an_unlisted_segment_that_is_not_the_last() {
    let dir = scratch_dir("unlisted");
    write(&dir, 2, &[cmd(0), cmd(1), cmd(2)]);
    fs::remove_file(dir.join(MANIFEST)).unwrap();
    let err = SegmentedLog::open(&dir).err().unwrap();
    assert!(format!("{err:#}").contains(&format!("is not in {MANIFEST}")), "{err:#}");
}

#[test]
fn opening_refuses_a_segment_named_for_another_seq_or_ending_mid_line() {
    let dir = scratch_dir("misnamed");
    write(&dir, 2, &[cmd(0), cmd(1), cmd(2)]);
    fs::rename(dir.join(SEG_2), dir.join("00000000000000000003.jsonl")).unwrap();
    let err = SegmentedLog::open(&dir).err().unwrap();
    assert!(format!("{err:#}").contains("does not start with the command for seq 3"), "{err:#}");

    let dir = scratch_dir("torn");
    write(&dir, 2, &[cmd(0)]);
    append(&dir.join(SEG_0), b"{\"entry\":");
    let err = SegmentedLog::open(&dir).err().unwrap();
    assert!(format!("{err:#}").contains("ends mid-line"), "{err:#}");
}

#[test]
fn a_log_written_as_one_file_before_segments_continues_as_the_first_segment() {
    let dir = scratch_dir("legacy");
    fs::create_dir_all(&dir).unwrap();
    let legacy = concat!(
        r#"{"entry":"command","seq":0,"ts":0,"cmd":{"type":"tick"}}"#,
        "\n",
        r#"{"entry":"checkpoint","seq":0,"root":"0x00","signature":"0x01","signer":"0x02"}"#,
        "\n",
    );
    fs::write(dir.join(SEG_0), legacy).unwrap();
    write(&dir, 2, &[cmd(1), cmd(2)]);
    let entries = SegmentedLog::open(&dir).unwrap().entries;
    assert!(matches!(&entries[0], LogEntry::Command(c) if c.events.is_none()));
    assert_eq!(entries[2..], [cmd(1), cmd(2)]);
    assert_eq!(files(&dir), [SEG_0, SEG_2, MANIFEST]);
}

#[test]
fn the_reader_starts_at_any_offset_across_segments() {
    let dir = scratch_dir("reader-offsets");
    write(&dir, 2, &[cmd(0), cmd(1), withdrawal(1), checkpoint(1), cmd(2), cmd(3)]);
    let all = [(0, 0), (1, 0), (1, 1), (1, 2), (2, 0), (3, 0)];
    assert_eq!(read_from(&dir, 0, 0), all);
    assert_eq!(read_from(&dir, 1, 1), all[2..]);
    assert_eq!(read_from(&dir, 2, 0), all[4..]);
    assert_eq!(read_from(&dir, 1, 7), all[4..]);
    assert!(read_from(&dir, 9, 0).is_empty());
}

#[test]
fn the_reader_leaves_a_line_still_being_written_for_a_later_call() {
    let dir = scratch_dir("reader-partial");
    write(&dir, 4, &[cmd(0)]);
    let mut reader = LogReader::open(&dir, Offset::start_of(0), manifest_of(&dir)).unwrap();
    assert_eq!(reader.next_entry().unwrap().map(|(o, _)| o), Some(Offset::start_of(0)));
    assert!(reader.next_entry().unwrap().is_none());

    let mut line = serde_json::to_vec(&cmd(1)).unwrap();
    line.push(b'\n');
    let (head, tail) = line.split_at(10);
    append(&dir.join(SEG_0), head);
    assert!(reader.next_entry().unwrap().is_none());
    append(&dir.join(SEG_0), tail);
    assert_eq!(reader.next_entry().unwrap(), Some((Offset::start_of(1), cmd(1))));
}

#[test]
fn the_reader_follows_the_writer_into_a_new_segment() {
    let dir = scratch_dir("reader-follow");
    let (manifest_tx, manifest) = watch::channel(Manifest::default());
    let mut writer = SegmentedLog::open(&dir).unwrap().into_writer(2, manifest_tx).unwrap();
    let mut reader = LogReader::open(&dir, Offset::start_of(0), manifest).unwrap();
    assert!(reader.next_entry().unwrap().is_none());

    writer.append(&cmd(0)).unwrap();
    writer.flush().unwrap();
    assert_eq!(reader.next_entry().unwrap(), Some((Offset::start_of(0), cmd(0))));
    assert!(reader.next_entry().unwrap().is_none());
    for seq in 1..=4 {
        writer.append(&cmd(seq)).unwrap();
    }
    writer.flush().unwrap();
    let offsets: Vec<_> = std::iter::from_fn(|| reader.next_entry().unwrap()).map(|(o, _)| o.seq).collect();
    assert_eq!(offsets, [1, 2, 3, 4]);
}

#[test]
fn opening_refuses_a_log_missing_its_newest_listed_segment() {
    let dir = scratch_dir("missing-newest");
    write(&dir, 2, &[cmd(0), cmd(1), cmd(2), cmd(3), cmd(4)]);
    fs::remove_file(dir.join(SEG_4)).unwrap();
    fs::remove_file(dir.join(SEG_2)).unwrap();
    let err = SegmentedLog::open(&dir).err().unwrap();
    assert!(format!("{err:#}").contains(&format!("{MANIFEST} lists 2 segments but 1 are")), "{err:#}");
}

#[test]
fn the_manifest_gets_a_line_per_completed_segment_and_drops_a_torn_last_line() {
    let dir = scratch_dir("manifest-lines");
    write(&dir, 2, &[cmd(0), cmd(1), cmd(2), cmd(3), cmd(4)]);
    let text = fs::read_to_string(dir.join(MANIFEST)).unwrap();
    assert_eq!(text.lines().count(), 2);
    assert_eq!(text.as_bytes(), Manifest::load(&dir).unwrap().to_jsonl().unwrap());

    // A crash while listing leaves part of a line; it is dropped and trimmed before the next one.
    append(&dir.join(MANIFEST), b"{\"file\":\"0000");
    assert_eq!(Manifest::load(&dir).unwrap().segments.len(), 2);
    write(&dir, 2, &[cmd(5), cmd(6)]);
    let text = fs::read_to_string(dir.join(MANIFEST)).unwrap();
    assert_eq!(text.lines().count(), 3, "{text}");
    assert_eq!(Manifest::load(&dir).unwrap().segments[2].first_seq, 4);
}

#[test]
fn the_reader_refuses_a_completed_segment_that_differs_from_its_listing() {
    let dir = scratch_dir("reader-tampered");
    write(&dir, 2, &[cmd(0), cmd(1), cmd(2)]);
    let manifest = manifest_of(&dir);
    let path = dir.join(SEG_0);
    let text = fs::read_to_string(&path).unwrap();
    fs::write(&path, text.replacen("\"ts\":0,", "\"ts\":9,", 1)).unwrap();
    let err = LogReader::open(&dir, Offset::start_of(0), manifest).err().unwrap();
    assert!(format!("{err:#}").contains("does not match its listing"), "{err:#}");
}

#[test]
fn the_reader_refuses_a_completed_segment_that_ends_mid_line() {
    let dir = scratch_dir("reader-torn");
    fs::create_dir_all(&dir).unwrap();
    let mut torn = serde_json::to_vec(&cmd(0)).unwrap();
    torn.extend(b"\n{\"entry\":");
    fs::write(dir.join(SEG_0), torn).unwrap();
    fs::write(dir.join(SEG_2), [serde_json::to_vec(&cmd(2)).unwrap(), b"\n".to_vec()].concat()).unwrap();
    let mut reader = LogReader::open(&dir, Offset::start_of(0), manifest_of(&dir)).unwrap();
    assert_eq!(reader.next_entry().unwrap(), Some((Offset::start_of(0), cmd(0))));
    let err = reader.next_entry().err().unwrap();
    assert!(format!("{err:#}").contains("ends mid-line"), "{err:#}");
}

#[test]
fn the_reader_refuses_a_seq_gap_or_a_missing_start_of_the_log() {
    let dir = scratch_dir("reader-gap");
    write(&dir, 2, &[cmd(0), cmd(1), cmd(2), cmd(3), cmd(4), cmd(5)]);
    fs::remove_file(dir.join(SEG_2)).unwrap();
    for from in [Offset::start_of(0), Offset::start_of(2)] {
        let mut reader = LogReader::open(&dir, from, manifest_of(&dir)).unwrap();
        let err = std::iter::from_fn(|| reader.next_entry().transpose()).find_map(Result::err).unwrap();
        assert!(format!("{err:#}").contains("seq 4 follows seq 1"), "{err:#}");
    }

    fs::remove_file(dir.join(SEG_0)).unwrap();
    let err = LogReader::open(&dir, Offset::start_of(0), manifest_of(&dir)).err().unwrap();
    assert!(format!("{err:#}").contains("holds seq 0"), "{err:#}");
}

// ── Helpers ──

fn cmd(seq: u64) -> LogEntry {
    LogEntry::Command(CommandEntry {
        sequenced: Sequenced { seq, ts: seq as i64 * 10, cmd: Command::Tick },
        events: Some(Vec::new()),
    })
}

/// One event of each kind, and an order rejection for each reason. The matches stop compiling when
/// a variant is added, until it has a sample here.
fn every_event() -> Vec<Event> {
    let (account, amount, market) = (AccountId([1; 20]), Fixed::from_int(5), 2);
    let reasons = [
        Reject::UnknownMarket,
        Reject::UnknownOrder,
        Reject::InvalidOrder { reason: "qty".into() },
        Reject::MarketClosed,
        Reject::ReduceOnly,
        Reject::NoPrice { error: PriceError::Stale { fresh: 1, needed: 2 } },
        Reject::NoPrice { error: PriceError::Divergent { divergence_bps: 30 } },
        Reject::Slippage { quoted: amount, live: amount, slippage_bps: 5 },
        Reject::InsufficientMargin { equity: amount, required: amount },
        Reject::InsufficientFunds { available: amount },
        Reject::HouseCapacity,
        Reject::InvalidAmount,
        Reject::DuplicateReference,
    ];
    for reason in &reasons {
        match reason {
            Reject::UnknownMarket
            | Reject::UnknownOrder
            | Reject::InvalidOrder { .. }
            | Reject::MarketClosed
            | Reject::ReduceOnly
            | Reject::NoPrice { .. }
            | Reject::Slippage { .. }
            | Reject::InsufficientMargin { .. }
            | Reject::InsufficientFunds { .. }
            | Reject::HouseCapacity
            | Reject::InvalidAmount
            | Reject::DuplicateReference => {}
        }
    }
    let fill = Fill {
        account,
        order_id: 3,
        market,
        side: Side::Sell,
        qty: amount,
        price: amount,
        index: amount,
        spread_bps: 4,
        realised_pnl: -amount,
        profit_fee: Fixed::ZERO,
        position: amount,
        liquidation: true,
    };
    let mut events: Vec<_> =
        reasons.into_iter().map(|reason| Event::OrderRejected { account, client_id: 1, reason }).collect();
    events.extend([
        Event::Deposited { account, amount },
        Event::DepositRejected { account, amount, chain_ref: "0xab:1".into(), reason: Reject::DuplicateReference },
        Event::WithdrawalApproved { account, amount, nonce: 1 },
        Event::WithdrawalRejected { account, amount, reason: Reject::InsufficientFunds { available: amount } },
        Event::HouseCapitalMoved { amount, nav: amount },
        Event::OrderAccepted { account, client_id: 1, order_id: 3 },
        Event::OrderCancelled { account, order_id: 3, reason: CancelReason::Client },
        Event::OrderCancelled { account, order_id: 3, reason: CancelReason::Unfillable },
        Event::Fill(fill),
        Event::CloseOut { account, equity: amount, margin: amount },
        Event::BadDebt { account, amount },
        Event::Financing { account, market, amount: -amount },
        Event::MarketOpened { market },
        Event::MarketClosed { market },
        Event::StressChanged { market, level: 2 },
        Event::PricesRejected { market, reason: "stale quote".into() },
        Event::AdminApplied,
        Event::AdminRejected { reason: "bad params".into() },
    ]);
    for event in &events {
        match event {
            Event::Deposited { .. }
            | Event::DepositRejected { .. }
            | Event::WithdrawalApproved { .. }
            | Event::WithdrawalRejected { .. }
            | Event::HouseCapitalMoved { .. }
            | Event::OrderAccepted { .. }
            | Event::OrderRejected { .. }
            | Event::OrderCancelled { .. }
            | Event::Fill(_)
            | Event::CloseOut { .. }
            | Event::BadDebt { .. }
            | Event::Financing { .. }
            | Event::MarketOpened { .. }
            | Event::MarketClosed { .. }
            | Event::StressChanged { .. }
            | Event::PricesRejected { .. }
            | Event::AdminApplied
            | Event::AdminRejected { .. } => {}
        }
    }
    events
}

fn withdrawal(nonce: u64) -> LogEntry {
    LogEntry::Withdrawal(SignedWithdrawal {
        account: AccountId([1; 20]),
        amount: Fixed::from_int(5),
        nonce,
        chain_id: 1,
        vault: AccountId([2; 20]),
        signature: "0x01".into(),
    })
}

fn checkpoint(seq: u64) -> LogEntry {
    LogEntry::Checkpoint(Checkpoint { seq, root: "0x00".into(), signature: "0x01".into(), signer: "0x02".into() })
}

/// Appends `entries` through a writer reopened on `dir`.
fn write(dir: &Path, segment_commands: u64, entries: &[LogEntry]) {
    let manifest = watch::channel(Manifest::default()).0;
    let mut writer = SegmentedLog::open(dir).unwrap().into_writer(segment_commands, manifest).unwrap();
    for entry in entries {
        writer.append(entry).unwrap();
    }
    writer.flush().unwrap();
}

/// The file names in `dir`, sorted.
fn files(dir: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    names.sort();
    names
}

fn append(path: &Path, bytes: &[u8]) {
    fs::OpenOptions::new().append(true).open(path).unwrap().write_all(bytes).unwrap();
}

/// Every `(seq, index)` a reader opened at `seq:index` yields.
fn read_from(dir: &Path, seq: u64, index: u32) -> Vec<(u64, u32)> {
    let mut reader = LogReader::open(dir, Offset { seq, index }, manifest_of(dir)).unwrap();
    std::iter::from_fn(|| reader.next_entry().unwrap()).map(|(o, _)| (o.seq, o.index)).collect()
}

/// The manifest on disk, as the writer would publish it.
fn manifest_of(dir: &Path) -> watch::Receiver<Manifest> {
    watch::channel(Manifest::load(dir).unwrap()).1
}
