use std::collections::BTreeSet;
use std::io::{Read as _, Seek as _};
use std::sync::Mutex;
use std::sync::mpsc::{TryRecvError, channel};

use peryx_policy::{PolicyAction, PolicyDecisionState};
use rstest::rstest;

use crate::meta::fault::corrupt;
use crate::meta::policy_decision::advance_repository_generations;
use crate::meta::{
    MetaStore, NewPolicyDecision, POLICY_INPUT_GENERATION, PolicyDecisionQuery, PolicyDecisionRecord,
    PolicyDecisionStoreError,
};

use super::*;

fn decision(resource: &str, state: PolicyDecisionState, evaluated_at_unix: i64) -> NewPolicyDecision<'_> {
    NewPolicyDecision {
        repository: "private",
        resource,
        group: Some("1.0"),
        artifact: Some("artifact-1.0.bin"),
        source: Some("alpha"),
        action: PolicyAction::Serve,
        state,
        rule: None,
        reason: None,
        evaluated_at_unix,
        next_eligible_at_unix: None,
    }
}

fn allow(resource: &str) -> NewPolicyDecision<'_> {
    decision(resource, PolicyDecisionState::Allow, 0)
}

fn open(queue: DecisionQueue) -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("peryx.redb"))
        .unwrap()
        .with_policy_decision_queue(queue);
    (dir, store)
}

/// A queue whose limits never interfere with the behavior under test.
fn roomy() -> DecisionQueue {
    DecisionQueue::with_limits(64, 64, 1_000)
}

/// Newest first, as the audit query returns it.
fn history(store: &MetaStore) -> Vec<PolicyDecisionRecord> {
    store
        .query_policy_decisions(&PolicyDecisionQuery {
            limit: 100,
            ..PolicyDecisionQuery::default()
        })
        .unwrap()
        .decisions
        .into_iter()
        .map(|item| item.record)
        .collect()
}

fn resources(records: &[PolicyDecisionRecord]) -> Vec<&str> {
    records.iter().map(|record| record.resource.as_str()).collect()
}

/// Hands each formatted log line to the test, which waits on the channel instead of polling.
struct ChannelWriter(Sender<String>);

impl std::io::Write for ChannelWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        _ = self.0.send(String::from_utf8_lossy(buf).into_owned());
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Queues the decisions while a held write transaction keeps the writer from starting, so they
/// reach it together and the batch boundaries do not depend on timing.
fn enqueue_behind_the_write_lock(store: &MetaStore, decisions: &[NewPolicyDecision<'_>]) {
    let lock = store.db.begin_write().unwrap();
    for decision in decisions {
        store.enqueue_policy_decision(*decision).unwrap();
    }
    drop(lock);
    store.policy_decisions.wait_applied();
}

#[test]
fn test_a_batch_keeps_arrival_order_and_points_each_subject_at_its_last_record() {
    let (_dir, store) = open(roomy());

    enqueue_behind_the_write_lock(
        &store,
        &[
            decision("a", PolicyDecisionState::Allow, 1),
            decision("b", PolicyDecisionState::Allow, 2),
            decision("a", PolicyDecisionState::Deny, 3),
            decision("b", PolicyDecisionState::Wait, 4),
            decision("a", PolicyDecisionState::Wait, 5),
        ],
    );

    let records = history(&store);
    assert_eq!(
        (
            resources(&records),
            store
                .current_policy_decision(allow("a"))
                .unwrap()
                .map(|record| record.state),
            store
                .current_policy_decision(allow("b"))
                .unwrap()
                .map(|record| record.state),
        ),
        (
            vec!["a", "b", "a", "b", "a"],
            Some(PolicyDecisionState::Wait),
            Some(PolicyDecisionState::Wait),
        )
    );
    assert_eq!(
        records
            .iter()
            .map(|record| record.evaluated_at_unix)
            .collect::<Vec<_>>(),
        [5, 4, 3, 2, 1]
    );
}

#[rstest]
#[case::one_batch(5, 8, 1)]
#[case::exactly_full(4, 4, 1)]
#[case::split_by_the_maximum(5, 2, 3)]
#[case::single(1, 8, 1)]
fn test_a_batch_commits_in_one_transaction_up_to_its_maximum(
    #[case] decisions: usize,
    #[case] max_batch: usize,
    #[case] transactions: usize,
) {
    let (_dir, store) = open(DecisionQueue::with_limits(64, max_batch, 1_000));
    let names = (0..decisions)
        .map(|index| format!("resource-{index}"))
        .collect::<Vec<_>>();
    let queued = names.iter().map(|name| allow(name)).collect::<Vec<_>>();
    _ = store.take_write_transaction_count();

    enqueue_behind_the_write_lock(&store, &queued);

    // The held lock accounts for one transaction.
    assert_eq!(
        (history(&store).len(), store.take_write_transaction_count()),
        (decisions, transactions + 1)
    );
}

#[rstest]
#[case::across_batches(4, 3, 10)]
#[case::within_one_batch(2, 8, 5)]
fn test_history_stays_within_its_bound_and_keeps_the_newest_rows(
    #[case] history_limit: usize,
    #[case] max_batch: usize,
    #[case] decisions: usize,
) {
    let (_dir, store) = open(DecisionQueue::with_limits(64, max_batch, history_limit));
    let names = (0..decisions)
        .map(|index| format!("resource-{index}"))
        .collect::<Vec<_>>();
    let queued = names.iter().map(|name| allow(name)).collect::<Vec<_>>();

    enqueue_behind_the_write_lock(&store, &queued);

    let kept = history(&store);
    let expected = names
        .iter()
        .rev()
        .take(history_limit)
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert_eq!(resources(&kept), expected);
    assert!(
        store.current_policy_decision(allow("resource-0")).unwrap().is_some(),
        "an evicted audit row must keep its subject's current decision"
    );
}

/// Fills a one-slot queue behind a held write lock, then runs `read` on another thread. A read that
/// takes the barrier finds the queue full and logs it before it waits, so the log line proves the
/// read reached the barrier while nothing could yet be committed. A read without the barrier
/// returns without logging, which ends the log channel and fails the test.
#[rstest]
#[case::current(|store: &MetaStore| store.current_policy_decision(allow("queued")).unwrap().is_some())]
#[case::for_artifacts(|store: &MetaStore| {
    store
        .current_policy_decisions_for_artifacts("private", "queued", &["artifact-1.0.bin"])
        .unwrap()
        .len()
        == 1
})]
#[case::history(|store: &MetaStore| history(store).len() == 2)]
fn test_a_read_waits_for_the_decisions_queued_before_it(#[case] read: fn(&MetaStore) -> bool) {
    let (_dir, store) = open(DecisionQueue::with_limits(1, 1, 1_000));
    let lock = store.db.begin_write().unwrap();
    // Enqueueing the second returns only after the writer took the first, so the slot is full.
    store.enqueue_policy_decision(allow("first")).unwrap();
    store.enqueue_policy_decision(allow("queued")).unwrap();
    let (log, blocked) = channel();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(Mutex::new(ChannelWriter(log)))
        .finish();
    let reader = std::thread::spawn(move || tracing::subscriber::with_default(subscriber, || read(&store)));

    assert!(blocked.recv().unwrap().contains("queue is full"));
    drop(lock);

    assert!(reader.join().unwrap());
}

#[test]
fn test_a_read_sees_a_decision_queued_just_before_it() {
    let (_dir, store) = open(roomy());

    store.enqueue_policy_decision(allow("queued")).unwrap();

    assert_eq!(resources(&history(&store)), ["queued"]);
}

#[test]
fn test_a_direct_write_lands_after_the_decisions_already_queued() {
    let (_dir, store) = open(roomy());

    store.enqueue_policy_decision(allow("queued")).unwrap();
    store.record_policy_decision(allow("direct")).unwrap();

    assert_eq!(resources(&history(&store)), ["direct", "queued"]);
}

#[test]
fn test_a_full_queue_blocks_the_sender_until_the_writer_drains() {
    let (_dir, store) = open(DecisionQueue::with_limits(1, 1, 1_000));
    let lock = store.db.begin_write().unwrap();
    // The writer takes the first and waits on the lock; the second fills the one slot.
    store.enqueue_policy_decision(allow("first")).unwrap();
    store.enqueue_policy_decision(allow("second")).unwrap();
    let (finished, done) = channel();
    let (log, blocked) = channel();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(Mutex::new(ChannelWriter(log)))
        .finish();
    let sender = {
        let store = store.clone();
        std::thread::spawn(move || {
            tracing::subscriber::with_default(subscriber, || {
                store.enqueue_policy_decision(allow("third")).unwrap();
            });
            finished.send(()).unwrap();
        })
    };

    // The sender logs before it waits, so the line proves it reached the full queue.
    assert!(blocked.recv().unwrap().contains("queue is full"));
    assert_eq!(done.try_recv(), Err(TryRecvError::Empty));
    drop(lock);
    done.recv().unwrap();
    sender.join().unwrap();

    assert_eq!(resources(&history(&store)), ["third", "second", "first"]);
}

#[test]
fn test_shutdown_commits_the_queued_decisions_and_later_ones_are_written_directly() {
    let (_dir, store) = open(DecisionQueue::with_limits(2, 64, 1_000));
    let lock = store.db.begin_write().unwrap();
    // The writer holds the first and waits on the lock; the other two fill the queue.
    for resource in ["a", "b", "c"] {
        store.enqueue_policy_decision(allow(resource)).unwrap();
    }
    let (finished, done) = channel();
    let (log, blocked) = channel();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(Mutex::new(ChannelWriter(log)))
        .finish();
    let stopper = {
        let store = store.clone();
        std::thread::spawn(move || {
            tracing::subscriber::with_default(subscriber, || store.shutdown_policy_decisions());
            finished.send(()).unwrap();
        })
    };

    // The full queue holds the shutdown request, so the log line proves the call started and has not
    // returned: nothing can drain while the lock is held.
    assert!(blocked.recv().unwrap().contains("queue is full"));
    assert_eq!(done.try_recv(), Err(TryRecvError::Empty));
    drop(lock);
    done.recv().unwrap();
    stopper.join().unwrap();
    assert_eq!(resources(&history(&store)), ["c", "b", "a"]);

    _ = store.take_write_transaction_count();
    store.enqueue_policy_decision(allow("after")).unwrap();

    assert_eq!(
        (resources(&history(&store)), store.take_write_transaction_count()),
        (vec!["after", "c", "b", "a"], 1)
    );
}

#[test]
fn test_dropping_the_store_commits_the_queued_decisions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let store = MetaStore::open(&path).unwrap();
    let lock = store.db.begin_write().unwrap();
    store.enqueue_policy_decision(allow("queued")).unwrap();
    drop(lock);

    drop(store);

    assert_eq!(resources(&history(&MetaStore::open(&path).unwrap())), ["queued"]);
}

#[test]
fn test_a_store_that_never_records_starts_no_writer() {
    let (_dir, store) = open(roomy());

    store.policy_decisions.wait_applied();
    store.shutdown_policy_decisions();

    assert!(store.policy_decisions.sender.get().is_none());
}

/// A decision evaluated before its repository's inputs change must read as stale, even when the
/// writer commits it after the change.
#[test]
fn test_a_decision_keeps_the_generation_it_was_queued_under() {
    let (_dir, store) = open(roomy());
    let lock = store.db.begin_write().unwrap();
    store.enqueue_policy_decision(allow("queued")).unwrap();
    advance_repository_generations(&lock, &BTreeSet::from(["private".to_owned()])).unwrap();
    lock.commit().unwrap();

    let found = store
        .current_policy_decisions_for_artifacts("private", "queued", &["artifact-1.0.bin"])
        .unwrap();

    assert!(!found["artifact-1.0.bin"].fresh);
}

#[test]
fn test_enqueue_reports_an_unreadable_input_generation() {
    let (_dir, store) = open(roomy());
    corrupt(&store, POLICY_INPUT_GENERATION, "private", b"not json");

    let result = store.enqueue_policy_decision(allow("lost"));

    assert!(matches!(result, Err(PolicyDecisionStoreError::Store(_))), "{result:?}");
}

#[test]
fn test_the_writer_logs_a_store_failure_and_keeps_running() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    drop(MetaStore::open(&path).unwrap());
    let store = MetaStore::open_existing_read_only(&path).unwrap();
    let mut capture = tempfile::tempfile().unwrap();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(Mutex::new(capture.try_clone().unwrap()))
        .finish();

    tracing::subscriber::with_default(subscriber, || {
        for resource in ["first", "second"] {
            store.enqueue_policy_decision(allow(resource)).unwrap();
            store.policy_decisions.wait_applied();
        }
    });

    capture.rewind().unwrap();
    let mut log = String::new();
    capture.read_to_string(&mut log).unwrap();
    assert_eq!(log.matches("failed to record policy decisions").count(), 2, "{log}");
}

#[test]
fn test_the_channel_writer_forwards_each_write_and_flushes_without_effect() {
    use std::io::Write as _;

    let (log, lines) = channel();
    let mut writer = ChannelWriter(log);

    assert_eq!(writer.write(b"line").unwrap(), 4);
    writer.flush().unwrap();

    assert_eq!(lines.try_recv().unwrap(), "line");
    assert_eq!(lines.try_recv(), Err(TryRecvError::Empty));
}
