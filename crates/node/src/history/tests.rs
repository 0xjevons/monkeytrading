use super::*;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use engine_core::Fixed;
use engine_core::command::{Command, Sequenced};
use engine_core::types::Millis;
use rusqlite::Connection;
use tokio::time::timeout;

use crate::sequencer::StreamMsg;
use crate::test_support::{
    RETENTION, TRADER, TestHistory, TestSequencer, deposit, every_event, logged_history, scratch_dir, withdraw,
};

const A: AccountId = AccountId([0xa; 20]);
const B: AccountId = AccountId([0xb; 20]);

#[tokio::test]
async fn pages_run_newest_first_between_exclusive_cursors() {
    let reader = sample_index("pages");
    let all = every_page(&reader, A, None, 10).await;
    assert_eq!(cursors(&all), ["13:0", "11:1", "10:2", "10:0"]);
    assert_eq!(all[0].ts, 1_300);

    let first = page(&reader, A, None, None, None, 2).await;
    assert_eq!(cursors(&first.events), ["13:0", "11:1"]);
    assert_eq!(first.next, Some(at(11, 1)));
    let second = page(&reader, A, None, first.next, None, 2).await;
    assert_eq!(cursors(&second.events), ["10:2", "10:0"]);
    assert_eq!(second.next, None);

    let between = page(&reader, A, None, Some(at(13, 0)), Some(at(10, 0)), 10).await;
    assert_eq!(cursors(&between.events), ["11:1", "10:2"]);
    assert!(page(&reader, A, None, Some(at(10, 0)), None, 10).await.events.is_empty());
    assert_eq!(cursors(&every_page(&reader, B, None, 1).await), ["10:1"]);
    assert!(every_page(&reader, AccountId([0xc; 20]), None, 1).await.is_empty());
}

#[tokio::test]
async fn a_kind_filter_returns_only_events_of_that_kind() {
    let reader = sample_index("kinds");
    assert_eq!(cursors(&every_page(&reader, A, Some("deposited"), 1).await), ["13:0", "10:0"]);
    assert_eq!(cursors(&every_page(&reader, A, Some("financing"), 1).await), ["11:1"]);
    let before = page(&reader, A, Some("deposited"), Some(at(13, 0)), None, 1).await;
    assert_eq!(cursors(&before.events), ["10:0"]);
    assert_eq!(before.next, None);
    assert!(every_page(&reader, A, Some("fill"), 1).await.is_empty());
}

#[test]
fn every_event_that_names_an_account_is_indexed_under_it_by_its_type() {
    let mut kinds = BTreeSet::new();
    for event in every_event() {
        let json = serde_json::to_value(&event).unwrap();
        let named = json.get("account").map(|account| account.as_str().unwrap().to_owned());
        assert_eq!(account_of(&event).map(|account| account.to_string()), named, "{json}");
        if named.is_some() {
            kinds.insert(json["type"].as_str().unwrap().to_owned());
        }
    }
    assert_eq!(kinds, KINDS.iter().map(|kind| kind.to_string()).collect());
}

#[tokio::test]
async fn taking_the_same_commands_again_changes_nothing() {
    let path = file(&index_dir("idempotent"));
    let mut writer = Writer::open(&path, ms(RETENTION)).unwrap();
    writer.write(&sample_log()).unwrap();
    let reader = Reader::new(path.clone());
    let once = every_page(&reader, A, None, 10).await;
    writer.write(&sample_log()).unwrap();
    writer.write(&sample_log()[1..]).unwrap();
    assert_eq!(every_page(&reader, A, None, 10).await, once);
    assert_eq!(writer.last_command().unwrap().unwrap().sequenced.seq, 13);
}

#[tokio::test]
async fn events_older_than_the_retention_leave_the_index() {
    let path = file(&index_dir("retention"));
    let mut writer = Writer::open(&path, 1_000).unwrap();
    let deposit_a = || vec![Event::Deposited { account: A, amount: Fixed::from_int(1) }];
    writer.write(&[command(1, 0, deposit_a()), command(2, 500, deposit_a())]).unwrap();
    let reader = Reader::new(path);
    assert_eq!(cursors(&every_page(&reader, A, None, 10).await), ["2:0", "1:0"]);
    // The horizon is 1_500 - 1_000: the event at 0 leaves, the one at exactly 500 stays.
    writer.write(&[command(3, 1_500, deposit_a())]).unwrap();
    assert_eq!(cursors(&every_page(&reader, A, None, 10).await), ["3:0", "2:0"]);
}

#[test]
fn an_index_with_another_schema_version_is_refused() {
    let path = file(&index_dir("schema"));
    drop(Writer::open(&path, 1).unwrap());
    Connection::open(&path).unwrap().pragma_update(None, "user_version", 2).unwrap();
    let err = Writer::open(&path, 1).err().expect("refused");
    assert!(format!("{err:#}").contains("history schema 2"), "{err:#}");
}

#[tokio::test]
async fn the_index_follows_the_log_and_resumes_after_a_restart() {
    let (dir, index) = (scratch_dir("history-restart"), index_dir("restart"));
    let node = TestSequencer::start(&dir, 1024);
    let history = TestHistory::start(&node, &dir, &index).unwrap();
    node.submit(deposit(100, "dep-1")).await;
    node.submit(withdraw(40)).await;
    node.stop().await;
    history.finish().await.unwrap();
    let reader = Reader::new(file(&index));
    let first = every_page(&reader, TRADER, None, 1).await;
    assert_eq!(first, logged_history(&dir, TRADER));
    assert_eq!(first.len(), 2);

    let node = TestSequencer::start(&dir, 1024);
    let history = TestHistory::start(&node, &dir, &index).unwrap();
    node.submit(withdraw(10)).await;
    node.stop().await;
    history.finish().await.unwrap();
    let second = every_page(&reader, TRADER, None, 1).await;
    assert_eq!(second, logged_history(&dir, TRADER));
    assert_eq!(second.len(), 3);
    assert_eq!(second[1..], first[..]);
}

#[tokio::test]
async fn a_removed_index_is_rebuilt_from_the_log_segments() {
    let (dir, index) = (scratch_dir("history-rebuild"), index_dir("rebuild"));
    let node = TestSequencer::start(&dir, 1024);
    let history = TestHistory::start(&node, &dir, &index).unwrap();
    for n in 0..10 {
        node.submit(deposit(1, &format!("dep-{n}"))).await;
    }
    node.stop().await;
    history.finish().await.unwrap();
    let reader = Reader::new(file(&index));
    let served = every_page(&reader, TRADER, None, 3).await;
    assert_eq!(served.len(), 10);

    fs::remove_dir_all(&index).unwrap();
    let node = TestSequencer::start(&dir, 1024);
    assert!(node.manifest.borrow().segments.len() > 2, "the log spans several completed segments");
    let history = TestHistory::start(&node, &dir, &index).unwrap();
    node.stop().await;
    history.finish().await.unwrap();
    assert_eq!(every_page(&reader, TRADER, None, 3).await, served);
}

#[tokio::test]
async fn an_index_ahead_of_its_log_is_refused() {
    let (dir, index) = (scratch_dir("history-ahead"), index_dir("ahead"));
    indexed_log(&dir, &index, 2).await;
    // The log as it stands now, which a host that rolls the log back would restore.
    let older = scratch_dir("history-ahead-older");
    fs::create_dir_all(&older).unwrap();
    for file in fs::read_dir(&dir).unwrap() {
        let file = file.unwrap();
        fs::copy(file.path(), older.join(file.file_name())).unwrap();
    }
    indexed_log(&dir, &index, 4).await;
    let node = TestSequencer::start(&older, 1024);
    let err = TestHistory::start(&node, &older, &index).err().expect("refused");
    assert!(format!("{err:#}").contains("the log ends before seq"), "{err:#}");
}

#[tokio::test]
async fn an_index_built_from_another_log_is_refused() {
    let (dir, index) = (scratch_dir("history-other-log"), index_dir("other-log"));
    indexed_log(&dir, &index, 2).await;
    // Another log of the same length, with other commands from genesis on.
    let other = scratch_dir("history-other-log-2");
    let node = TestSequencer::start(&other, 1024);
    for n in 0..4 {
        node.submit(deposit(7, &format!("other-{n}"))).await;
    }
    let err = TestHistory::start(&node, &other, &index).err().expect("refused");
    assert!(format!("{err:#}").contains("is not the command the index took"), "{err:#}");
}

#[tokio::test]
async fn the_sequencer_runs_on_while_the_index_writer_waits_on_its_file() {
    let (dir, index) = (scratch_dir("history-stall"), index_dir("stall"));
    let node = TestSequencer::start(&dir, 1024);
    let history = TestHistory::start(&node, &dir, &index).unwrap();
    let lock = Connection::open(file(&index)).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    let sequenced = async {
        for n in 0..50 {
            node.submit(deposit(1, &format!("dep-{n}"))).await;
        }
    };
    timeout(Duration::from_secs(3), sequenced).await.expect("the sequencer does not wait on the index");
    let reader = Reader::new(file(&index));
    assert!(every_page(&reader, TRADER, None, 200).await.is_empty(), "the writer took nothing while blocked");
    lock.execute_batch("ROLLBACK").unwrap();
    node.stop().await;
    history.finish().await.unwrap();
    assert_eq!(every_page(&reader, TRADER, None, 200).await.len(), 50);
}

#[tokio::test]
async fn a_failed_index_write_stops_the_history_task() {
    let (dir, index) = (scratch_dir("history-fail"), index_dir("fail"));
    let node = TestSequencer::start(&dir, 1024);
    let history = TestHistory::start(&node, &dir, &index).unwrap();
    Connection::open(file(&index)).unwrap().execute_batch("DROP TABLE events").unwrap();
    node.submit(deposit(1, "dep-1")).await;
    let err = history.finish().await.expect_err("the write fails");
    assert!(format!("{err:#}").contains("no such table: events"), "{err:#}");
}

// ── Helpers ──

/// A fresh index directory for one test.
fn index_dir(name: &str) -> PathBuf {
    scratch_dir(&format!("history-index-{name}"))
}

/// The index file in the index directory `index`.
fn file(index: &Path) -> PathBuf {
    index.join(INDEX_FILE)
}

fn ms(duration: Duration) -> Millis {
    Millis::try_from(duration.as_millis()).unwrap()
}

fn at(seq: u64, index: u32) -> Offset {
    Offset { seq, index }
}

/// A command message at `seq`, stamped `ts`, that produced `events`.
fn command(seq: u64, ts: Millis, events: Vec<Event>) -> Arc<StreamMsg> {
    let sequenced = Sequenced { seq, ts, cmd: Command::Tick };
    Arc::new(StreamMsg::new(Offset::start_of(seq), LogEntry::Command(CommandEntry { sequenced, events: Some(events) })))
}

/// Commands with events for `A` at 10:0, 10:2, 11:1, and 13:0, and for `B` at 10:1.
fn sample_log() -> Vec<Arc<StreamMsg>> {
    let amount = Fixed::from_int(1);
    vec![
        command(
            10,
            1_000,
            vec![
                Event::Deposited { account: A, amount },
                Event::Deposited { account: B, amount },
                Event::WithdrawalApproved { account: A, amount, nonce: 1 },
            ],
        ),
        command(11, 1_100, vec![Event::MarketOpened { market: 1 }, Event::Financing { account: A, market: 1, amount }]),
        command(12, 1_200, Vec::new()),
        command(13, 1_300, vec![Event::Deposited { account: A, amount }]),
    ]
}

/// A reader of an index that took `sample_log`.
fn sample_index(name: &str) -> Reader {
    let path = file(&index_dir(name));
    Writer::open(&path, ms(RETENTION)).unwrap().write(&sample_log()).unwrap();
    Reader::new(path)
}

/// Runs a node on `dir` with an index in `index` through `deposits` deposits, then stops both.
async fn indexed_log(dir: &Path, index: &Path, deposits: usize) {
    let node = TestSequencer::start(dir, 1024);
    let history = TestHistory::start(&node, dir, index).unwrap();
    for n in 0..deposits {
        node.submit(deposit(1, &format!("dep-{n}"))).await;
    }
    node.stop().await;
    history.finish().await.unwrap();
}

async fn page(
    reader: &Reader,
    account: AccountId,
    kind: Option<&str>,
    before: Option<Offset>,
    after: Option<Offset>,
    limit: usize,
) -> Page {
    let query = PageQuery { account, kind: kind.map(str::to_owned), before, after, limit };
    reader.page(query).await.unwrap()
}

/// Every page from the newest, following `next`. Each page must hold only events older than the
/// `before` it was asked for, so a page that does not move on fails rather than repeats.
async fn every_page(reader: &Reader, account: AccountId, kind: Option<&str>, limit: usize) -> Vec<HistoryEvent> {
    let (mut events, mut before): (Vec<HistoryEvent>, Option<Offset>) = (Vec::new(), None);
    loop {
        let page = page(reader, account, kind, before, None, limit).await;
        assert!(page.events.len() <= limit);
        let older = |cursor: Offset| before.is_none_or(|before| cursor < before);
        assert!(
            page.events.iter().all(|e| older(e.cursor)) && page.next.is_none_or(older),
            "{page:?} after {before:?}"
        );
        events.extend(page.events);
        match page.next {
            Some(next) => before = Some(next),
            None => return events,
        }
    }
}

fn cursors(events: &[HistoryEvent]) -> Vec<String> {
    events.iter().map(|event| event.cursor.to_string()).collect()
}
