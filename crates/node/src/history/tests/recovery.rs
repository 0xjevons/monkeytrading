//! Startup checks against the log, readiness to serve, and failures.

use super::*;

#[test]
fn an_index_with_another_schema_version_is_refused() {
    let path = file(&index_dir("schema"));
    drop(Writer::open(&path, 1).unwrap());
    Connection::open(&path).unwrap().pragma_update(None, "user_version", 2).unwrap();
    let err = Writer::open(&path, 1).err().expect("refused");
    assert!(format!("{err:#}").contains("history schema 2"), "{err:#}");
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
    let err = TestHistory::start(&node, &older, &index).await.err().expect("refused");
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
    let err = TestHistory::start(&node, &other, &index).await.err().expect("refused");
    assert!(format!("{err:#}").contains("holds another command there"), "{err:#}");
}

/// Pages wait until the index has taken every command the log held when it started: a rebuild over
/// a logged history and a restart behind the log answer `None` until then, while a restart with
/// nothing left to take serves at once.
#[tokio::test]
async fn pages_wait_until_the_index_has_taken_the_log_it_started_on() {
    // (case, deposits indexed before the restart, deposits logged while the index is down, served at once)
    for (case, indexed, while_down, at_once) in
        [("rebuild", None, 3, false), ("behind", Some(2), 3, false), ("caught-up", Some(2), 0, true)]
    {
        let (dir, index) = (scratch_dir(&format!("history-ready-{case}")), index_dir(&format!("ready-{case}")));
        let node = TestSequencer::start(&dir, 1024);
        match indexed {
            Some(deposits) => {
                let history = TestHistory::start(&node, &dir, &index).await.unwrap();
                for n in 0..deposits {
                    node.submit(deposit(1, &format!("dep-{n}"))).await;
                }
                node.stop().await;
                history.finish().await.unwrap();
            }
            None => {
                node.stop().await;
                drop(Writer::open(&file(&index), ms(RETENTION)).unwrap());
            }
        }
        let node = TestSequencer::start(&dir, 1024);
        for n in 0..while_down {
            node.submit(deposit(1, &format!("down-{n}"))).await;
        }
        let tip = node.next_seq().await.checked_sub(1);
        // The writer cannot commit while this is held, so the index cannot catch up.
        let lock = Connection::open(file(&index)).unwrap();
        lock.execute_batch("BEGIN IMMEDIATE").unwrap();
        let history = TestHistory::start_with_tip(&node, &dir, &index, tip).unwrap();
        let query = || PageQuery { account: TRADER, kind: None, before: None, after: None, limit: 200 };
        assert_eq!(history.reader.page(query()).await.unwrap().is_some(), at_once, "{case}");
        lock.execute_batch("ROLLBACK").unwrap();
        let reader = history.reader.clone();
        node.stop().await;
        history.finish().await.unwrap();
        let page = reader.page(query()).await.unwrap().expect("caught up");
        assert_eq!(page.events, logged_history(&dir, TRADER), "{case}");
    }
}

/// The writer lets readers in once it has written a command at or past the tip it was given, and
/// never before; without a tip it leaves the flag to `catch_up_to`.
#[test]
fn the_writer_lets_readers_in_once_it_writes_the_tip() {
    // (tip, commands written, readers let in)
    for (tip, seqs, let_in) in
        [(Some(5), [3, 4], false), (Some(5), [4, 5], true), (Some(5), [6, 7], true), (None, [1, 2], false)]
    {
        let writer = Writer::open(&file(&index_dir("writer-ready")), ms(RETENTION)).unwrap();
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        seqs.iter().for_each(|seq| tx.try_send(command(*seq, 0, Vec::new())).unwrap());
        drop(tx);
        let (ready, readers) = watch::channel(false);
        writer.run(rx, ready, tip).unwrap();
        assert_eq!(*readers.borrow(), let_in, "tip {tip:?}, wrote {seqs:?}");
    }
}

/// `catch_up_to` lets readers in at once only when the index already holds the tip, waits when it
/// has yet to take it, and refuses a recovered log that ends before the index's last command.
#[tokio::test]
async fn readers_wait_for_a_tip_the_index_has_not_taken() {
    let (dir, index) = (scratch_dir("history-tip"), index_dir("tip"));
    indexed_log(&dir, &index, 1).await;
    let next = last_command_seq(&dir) + 1;
    let cfg = HistorySection { dir: index.clone(), retention: RETENTION };
    // (tip, Some(readers let in at once), or None when refused)
    let cases = [
        (None, None),
        (Some(next - 2), None),
        (Some(next - 1), Some(true)),
        (Some(next), Some(false)),
        (Some(next + 1), Some(false)),
    ];
    for (tip, outcome) in cases {
        let mut history = History::open(&cfg, &dir).unwrap();
        assert_eq!(history.resume_at, Offset::start_of(next));
        match (history.catch_up_to(tip), outcome) {
            (Ok(()), Some(at_once)) => assert_eq!(*history.ready.borrow(), at_once, "tip {tip:?}"),
            (Err(err), None) => assert!(format!("{err:#}").contains("ends before seq"), "{err:#}"),
            (result, _) => panic!("tip {tip:?}: {result:?}"),
        }
    }
    // A fresh index on an empty log has nothing to take.
    let mut fresh = History::open(&HistorySection { dir: index_dir("tip-fresh"), retention: RETENTION }, &dir).unwrap();
    fresh.catch_up_to(None).unwrap();
    assert!(*fresh.ready.borrow());
}

/// A history failure carries its cause: an unusable index directory, a damaged log segment at the
/// startup check (blamed on the log, not the index), and one the feed meets while catching up.
#[tokio::test]
async fn a_history_failure_names_its_cause() {
    let (dir, index) = (scratch_dir("history-causes"), index_dir("causes"));
    let occupied = scratch_dir("history-causes-file");
    File::create(&occupied).unwrap();
    let cfg = HistorySection { dir: occupied.join("history"), retention: RETENTION };
    let err = History::open(&cfg, &dir).err().expect("no directory under a file");
    assert!(format!("{err:#}").contains("creating"), "{err:#}");

    indexed_log(&dir, &index, 1).await;
    let last = last_command_seq(&dir);
    // More commands complete the segment that holds the index's last command.
    let node = TestSequencer::start(&dir, 1024);
    for n in 0..SEGMENT_COMMANDS {
        node.submit(deposit(1, &format!("after-{n}"))).await;
    }
    node.stop().await;
    let manifest = Manifest::load(&dir).unwrap();
    let holding = manifest.segments.iter().find(|s| s.first_seq <= last && last <= s.last_seq).unwrap();
    OpenOptions::new().append(true).open(dir.join(&holding.file)).unwrap().write_all(b"\n").unwrap();
    let cfg = HistorySection { dir: index.clone(), retention: RETENTION };
    let err = format!("{:#}", History::open(&cfg, &dir).err().expect("a damaged segment"));
    assert!(err.contains("reading the log") && err.contains("does not match its listing"), "{err}");
    assert!(!err.contains("remove the directory"), "{err}");

    let fresh = HistorySection { dir: index_dir("causes-fresh"), retention: RETENTION };
    let mut history = History::open(&fresh, &dir).unwrap();
    history.catch_up_to(None).unwrap();
    let (live, subscriber) = broadcast::channel(1);
    drop(live);
    let backfills = Arc::new(Semaphore::new(MAX_BACKFILLS));
    let manifest = watch::channel(manifest).1;
    let feed = Feed::new(subscriber, dir.clone(), manifest, backfills, Some(history.resume_at()));
    let err = timeout(Duration::from_secs(10), history.run(feed)).await.unwrap().expect_err("the feed fails");
    assert!(format!("{err:#}").contains("does not match its listing"), "{err:#}");
}

#[tokio::test]
async fn an_index_that_does_not_parse_fails_with_its_cause() {
    let (dir, index) = (scratch_dir("history-corrupt"), index_dir("corrupt"));
    indexed_log(&dir, &index, 1).await;
    let edit = Connection::open(file(&index)).unwrap();
    edit.execute_batch("UPDATE events SET event = 'not json'").unwrap();
    let query = PageQuery { account: TRADER, kind: None, before: None, after: None, limit: 10 };
    let err = Reader::new(file(&index), ready()).page(query).await.expect_err("an unreadable event");
    assert!(format!("{err:#}").contains("history event"), "{err:#}");

    edit.execute_batch("UPDATE progress SET command = 'not json'").unwrap();
    let node = TestSequencer::start(&dir, 16);
    let err = TestHistory::start(&node, &dir, &index).await.err().expect("unreadable progress");
    assert!(format!("{err:#}").contains("history progress"), "{err:#}");

    let err = Writer::open(Path::new(":memory:"), 1).err().expect("no write-ahead log");
    assert!(format!("{err:#}").contains("cannot use a write-ahead log"), "{err:#}");

    let junk = file(&index_dir("corrupt-junk"));
    fs::create_dir_all(junk.parent().unwrap()).unwrap();
    fs::write(&junk, "not a SQLite database, but long enough for SQLite to read its header and say so").unwrap();
    let err = Writer::open(&junk, 1).err().expect("not a database");
    assert!(format!("{err:#}").contains("not a database"), "{err:#}");

    let (dir, index) = (scratch_dir("history-corrupt-row"), index_dir("corrupt-row"));
    indexed_log(&dir, &index, 1).await;
    Connection::open(file(&index)).unwrap().execute_batch("UPDATE events SET seq = -1").unwrap();
    let query = PageQuery { account: TRADER, kind: None, before: None, after: None, limit: 10 };
    let err = Reader::new(file(&index), ready()).page(query).await.expect_err("a row out of range");
    assert!(format!("{err:#}").contains("history row -1:0"), "{err:#}");
}

#[tokio::test]
async fn a_failed_index_write_stops_the_history_task() {
    let (dir, index) = (scratch_dir("history-fail"), index_dir("fail"));
    let node = TestSequencer::start(&dir, 1024);
    let history = TestHistory::start(&node, &dir, &index).await.unwrap();
    Connection::open(file(&index)).unwrap().execute_batch("DROP TABLE events").unwrap();
    node.submit(deposit(1, "dep-1")).await;
    let err = history.finish().await.expect_err("the write fails");
    assert!(format!("{err:#}").contains("did not create"), "{err:#}");
}
