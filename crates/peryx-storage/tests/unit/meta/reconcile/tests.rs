use super::{NewReconcileEntry, ReconcileEnqueue};
use crate::meta::MetaStore;

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
    // A re-scan of the same operation resolves the existing entry rather than staging it twice.
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
    // A re-run of the drain never re-settles a terminal entry, and an unknown key is a no-op.
    assert!(!meta.settle_reconcile("east:4:1", "superseded", NOW + 5).unwrap());
    assert!(!meta.settle_reconcile("east:4:9", "failed", NOW).unwrap());

    let record = meta.reconcile_entry("east:4:1").unwrap().unwrap();
    assert_eq!(record.outcome.as_deref(), Some("replayable"));
    assert!(meta.pending_reconcile(10).unwrap().is_empty());
}

#[test]
fn test_prune_releases_only_settled_entries_past_both_frontiers() {
    let dir = tempfile::tempdir().unwrap();
    let meta = store(&dir);
    for serial in [1, 2, 3] {
        meta.enqueue_reconcile(&entry(serial), NOW).unwrap();
    }
    meta.settle_reconcile("east:4:1", "replayable", NOW).unwrap();
    meta.settle_reconcile("east:4:2", "superseded", NOW).unwrap();
    // 3 stays pending.

    // The audit-retention frontier still trails serial 2, so only serial 1 releases.
    assert_eq!(meta.prune_reconcile(10, 1, 10).unwrap(), 1);
    assert!(meta.reconcile_entry("east:4:1").unwrap().is_none());
    // A replica frontier that trails holds everything else.
    assert_eq!(
        meta.prune_reconcile(1, 10, 10).unwrap(),
        0,
        "serial 2 still trailed by the replica"
    );
    // Both frontiers past serial 2 releases it; the pending serial 3 is never pruned.
    assert_eq!(meta.prune_reconcile(10, 10, 10).unwrap(), 1);
    assert!(meta.reconcile_entry("east:4:2").unwrap().is_none());
    assert!(meta.reconcile_entry("east:4:3").unwrap().unwrap().is_pending());
}

#[test]
fn test_prune_bounds_its_batch() {
    let dir = tempfile::tempdir().unwrap();
    let meta = store(&dir);
    for serial in 1..=4 {
        meta.enqueue_reconcile(&entry(serial), NOW).unwrap();
        meta.settle_reconcile(&format!("east:4:{serial}"), "failed", NOW)
            .unwrap();
    }
    assert_eq!(
        meta.prune_reconcile(10, 10, 2).unwrap(),
        2,
        "one batch releases at most the limit"
    );
    assert_eq!(meta.count_reconcile().unwrap(), 2);
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
        // Drain a bounded first batch, then the process stops mid-backlog.
        for (key, _) in meta.pending_reconcile(2).unwrap() {
            assert!(meta.settle_reconcile(&key, "replayable", NOW).unwrap());
        }
    }

    // A fresh process reopens the durable backlog and resumes: the two settled entries are skipped,
    // and re-enqueueing an operation the crash may have re-scanned never resets its outcome.
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
    // Exactly once: none of the five re-settles.
    for serial in 1..=5 {
        assert!(
            !meta
                .settle_reconcile(&format!("east:4:{serial}"), "failed", NOW + 200)
                .unwrap()
        );
    }
}
