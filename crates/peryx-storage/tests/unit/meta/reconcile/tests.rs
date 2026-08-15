use super::{NewReconcileEntry, ReconcileEnqueue, ReconcileStore};
use crate::meta::MetaStore;
use std::num::NonZeroUsize;

const NOW: i64 = 1_800_000_000;

fn store(dir: &tempfile::TempDir) -> MetaStore {
    MetaStore::open(dir.path().join("meta.redb")).unwrap()
}

fn entry(serial: u64) -> NewReconcileEntry<'static> {
    NewReconcileEntry {
        source: "east",
        epoch: 4,
        serial,
        durably_committed: true,
        already_applied: false,
        superseded: false,
        traceparent: None,
    }
}

#[test]
fn test_enqueue_stages_a_pending_entry_once() {
    let dir = tempfile::tempdir().unwrap();
    let meta = store(&dir);

    assert_eq!(
        meta.enqueue_reconcile(&entry(1), NOW).unwrap(),
        ReconcileEnqueue::Enqueued
    );
    assert_eq!(
        meta.enqueue_reconcile(&entry(1), NOW + 10).unwrap(),
        ReconcileEnqueue::AlreadyPresent
    );
    assert_eq!(meta.count_reconcile().unwrap(), 1);
    let record = meta.reconcile_entry("east:4:1").unwrap().unwrap();
    assert!(record.is_pending());
    assert_eq!(record.updated_at_unix, NOW, "the first entry stands unchanged");
}

#[test]
fn test_pending_reads_in_key_order_within_the_limit() {
    let dir = tempfile::tempdir().unwrap();
    let meta = store(&dir);
    for serial in [3, 1, 2] {
        meta.enqueue_reconcile(&entry(serial), NOW).unwrap();
    }

    let batch = meta.pending_reconcile(2).unwrap();
    let keys: Vec<_> = batch.iter().map(|(key, _)| key.as_str()).collect();
    assert_eq!(keys, ["east:4:1", "east:4:2"], "bounded and ordered");
}

#[test]
fn test_settle_stamps_once_and_never_re_settles() {
    let dir = tempfile::tempdir().unwrap();
    let meta = store(&dir);
    meta.enqueue_reconcile(&entry(1), NOW).unwrap();

    assert!(meta.settle_reconcile("east:4:1", "replayable", NOW).unwrap());
    assert!(!meta.settle_reconcile("east:4:1", "superseded", NOW + 5).unwrap());
    assert!(!meta.settle_reconcile("east:4:9", "failed", NOW).unwrap());

    let record = meta.reconcile_entry("east:4:1").unwrap().unwrap();
    assert_eq!(record.outcome.as_deref(), Some("replayable"));
    assert!(meta.pending_reconcile(10).unwrap().is_empty());
}

#[test]
fn test_scan_paginates_records_and_reads_status_partitions() {
    let dir = tempfile::tempdir().unwrap();
    let meta = store(&dir);
    for serial in [1, 2, 3] {
        meta.enqueue_reconcile(&entry(serial), NOW).unwrap();
    }
    meta.settle_reconcile("east:4:1", "replayable", NOW).unwrap();
    meta.settle_reconcile("east:4:2", "superseded", NOW).unwrap();

    assert_eq!(
        meta.scan_reconcile(None, NonZeroUsize::new(2).unwrap())
            .unwrap()
            .records
            .into_iter()
            .map(|(key, _)| key)
            .collect::<Vec<_>>(),
        ["east:4:1", "east:4:2"]
    );
    assert_eq!(
        meta.scan_reconcile(Some("east:4:2"), NonZeroUsize::new(2).unwrap())
            .unwrap()
            .records
            .into_iter()
            .map(|(key, _)| key)
            .collect::<Vec<_>>(),
        ["east:4:3"]
    );
    assert_eq!(meta.settled_reconcile(10).unwrap().len(), 2);
    assert_eq!(meta.pending_reconcile(10).unwrap().len(), 1);
}

#[test]
fn test_scan_reads_empty_state_before_the_table_exists() {
    let dir = tempfile::tempdir().unwrap();
    let meta = store(&dir);

    let page = meta.scan_reconcile(None, NonZeroUsize::MIN).unwrap();

    assert!(page.records.is_empty());
    assert_eq!(page.next_cursor, None);
}

#[test]
fn test_compare_and_remove_requires_the_complete_record() {
    let dir = tempfile::tempdir().unwrap();
    let meta = store(&dir);
    meta.enqueue_reconcile(&entry(1), NOW).unwrap();
    let expected = meta.reconcile_entry("east:4:1").unwrap().unwrap();
    let mut stale = expected.clone();
    stale.updated_at_unix += 1;

    assert!(!meta.compare_and_remove_reconcile("east:4:1", &stale).unwrap());
    assert!(meta.compare_and_remove_reconcile("east:4:1", &expected).unwrap());
    assert!(!meta.compare_and_remove_reconcile("east:4:1", &expected).unwrap());
}

#[test]
fn test_drain_resumes_across_a_restart_and_settles_each_op_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("meta.redb");
    {
        let meta = MetaStore::open(&path).unwrap();
        for serial in 1..=5 {
            meta.enqueue_reconcile(&entry(serial), NOW).unwrap();
        }
        for (key, _) in meta.pending_reconcile(2).unwrap() {
            assert!(meta.settle_reconcile(&key, "replayable", NOW).unwrap());
        }
    }

    let meta = MetaStore::open(&path).unwrap();
    assert_eq!(
        meta.enqueue_reconcile(&entry(1), NOW + 100).unwrap(),
        ReconcileEnqueue::AlreadyPresent
    );
    let resumed = meta.pending_reconcile(10).unwrap();
    assert_eq!(resumed.len(), 3, "only the un-settled operations remain to reconcile");

    for (key, _) in resumed {
        assert!(meta.settle_reconcile(&key, "replayable", NOW + 100).unwrap());
    }
    assert!(
        meta.pending_reconcile(10).unwrap().is_empty(),
        "every operation reached a terminal outcome"
    );
    for serial in 1..=5 {
        assert!(
            !meta
                .settle_reconcile(&format!("east:4:{serial}"), "failed", NOW + 200)
                .unwrap()
        );
    }
}

#[test]
fn test_reconcile_trait_delegates_the_store_contract() {
    let dir = tempfile::tempdir().unwrap();
    let meta = store(&dir);
    let entry = entry(1);

    assert_eq!(
        <MetaStore as ReconcileStore>::enqueue_reconcile(&meta, &entry, NOW).unwrap(),
        ReconcileEnqueue::Enqueued
    );
    assert_eq!(
        <MetaStore as ReconcileStore>::pending_reconcile(&meta, 1)
            .unwrap()
            .len(),
        1
    );
    assert!(
        <MetaStore as ReconcileStore>::settled_reconcile(&meta, 1)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        <MetaStore as ReconcileStore>::scan_reconcile(&meta, None, NonZeroUsize::MIN)
            .unwrap()
            .records
            .len(),
        1
    );
    let key = entry.key();
    let record = <MetaStore as ReconcileStore>::reconcile_entry(&meta, &key)
        .unwrap()
        .unwrap();
    assert!(<MetaStore as ReconcileStore>::settle_reconcile(&meta, &key, "applied", NOW + 1).unwrap());
    assert!(!<MetaStore as ReconcileStore>::compare_and_remove_reconcile(&meta, &key, &record).unwrap());
}
