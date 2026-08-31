use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use peryx_ha::{ReclaimGuard, ReclaimGuardArm, ReclaimGuardStore as _};
use rstest::rstest;

use crate::meta::{DriverBlobReference, DriverMutation, JournalEntry, MetaError, MetaStore};

fn guard(expires_at_unix: i64) -> ReclaimGuard {
    ReclaimGuard { expires_at_unix }
}

/// Admission judges a lease against the store's clock, so these tests step it instead of waiting.
fn stepped_store() -> (tempfile::TempDir, MetaStore, Arc<AtomicI64>) {
    let dir = tempfile::tempdir().unwrap();
    let now = Arc::new(AtomicI64::new(0));
    let ticks = Arc::clone(&now);
    let store = MetaStore::open(dir.path().join("peryx.redb"))
        .unwrap()
        .with_clock(Arc::new(move || ticks.load(Ordering::Relaxed)));
    (dir, store, now)
}

fn publish_reference(store: &MetaStore, digest: &str) -> Result<(), MetaError> {
    store.commit_driver_txn(|txn| {
        txn.put("ref/1", b"points-here")?;
        txn.reference_blob(digest, 6);
        Ok::<_, MetaError>(((), vec![b"{}".to_vec()]))
    })
}

#[test]
fn test_live_blob_reclaim_guard_blocks_a_reference_and_admits_it_after_deletion() {
    let (_dir, store, now) = stepped_store();
    now.store(10, Ordering::Relaxed);
    let digest = "orphaned-blob-digest";
    assert_eq!(
        store
            .compare_and_arm_reclaim_guards(&[digest], 0, 10, guard(11))
            .unwrap(),
        ReclaimGuardArm::Armed(vec![digest.to_owned()])
    );
    assert_eq!(store.reclaim_guard(digest).unwrap(), Some(guard(11)));

    let error = publish_reference(&store, digest).unwrap_err();
    assert!(matches!(error, MetaError::BlobReclaiming { digest: rejected } if rejected == digest));
    assert!(
        store.get_driver_value("ref/1").unwrap().is_none(),
        "the rejected commit published nothing"
    );
    assert_eq!(
        store.current_serial().unwrap(),
        0,
        "the rejected commit advanced no serial"
    );

    assert!(store.compare_and_disarm_reclaim_guard(digest, guard(11)).unwrap());
    publish_reference(&store, digest).unwrap();
    assert_eq!(
        store.get_driver_value("ref/1").unwrap().as_deref(),
        Some(b"points-here".as_slice())
    );
}

#[rstest]
#[case::at_the_lease_deadline(11)]
#[case::past_the_lease_deadline(12)]
fn test_lapsed_blob_reclaim_guard_admits_a_reference_and_drops_its_row(#[case] now_unix: i64) {
    let (_dir, store, now) = stepped_store();
    let digest = "orphaned-blob-digest";
    store
        .compare_and_arm_reclaim_guards(&[digest], 0, 10, guard(11))
        .unwrap();
    now.store(now_unix, Ordering::Relaxed);

    publish_reference(&store, digest).unwrap();

    assert_eq!(
        store.get_driver_value("ref/1").unwrap().as_deref(),
        Some(b"points-here".as_slice())
    );
    assert_eq!(
        store.reclaim_guard(digest).unwrap(),
        None,
        "the commit that admitted past the lease also cleared it"
    );
}

#[test]
fn test_blob_reclaim_guard_on_another_digest_admits_an_unguarded_reference() {
    let (_dir, store, now) = stepped_store();
    now.store(10, Ordering::Relaxed);
    store
        .compare_and_arm_reclaim_guards(&["guarded"], 0, 10, guard(100))
        .unwrap();

    publish_reference(&store, "unguarded").unwrap();

    assert_eq!(
        store.get_driver_value("ref/1").unwrap().as_deref(),
        Some(b"points-here".as_slice())
    );
    assert_eq!(
        store.reclaim_guard("guarded").unwrap(),
        Some(guard(100)),
        "an unrelated live lease survives the commit"
    );
}

#[test]
fn test_default_store_clock_judges_a_blob_reclaim_guard_against_wall_time() {
    let (_dir, store) = super::store();
    store
        .compare_and_arm_reclaim_guards(&["lapsed"], 0, 0, guard(1))
        .unwrap();
    store
        .compare_and_arm_reclaim_guards(&["held"], 0, 0, guard(i64::MAX))
        .unwrap();

    publish_reference(&store, "lapsed").unwrap();
    let error = publish_reference(&store, "held").unwrap_err();

    assert!(matches!(error, MetaError::BlobReclaiming { digest } if digest == "held"));
    assert_eq!(store.reclaim_guard("lapsed").unwrap(), None);
    assert_eq!(store.reclaim_guard("held").unwrap(), Some(guard(i64::MAX)));
}

#[test]
fn test_arm_blob_reclaim_guards_refuses_a_reference_that_appends_no_journal_entry() {
    let (_dir, store) = super::store();
    store
        .commit_driver_txn(|txn| {
            txn.put("ref/kept", b"points-here")?;
            txn.reference_blob("kept", 1);
            Ok::<_, MetaError>(((), Vec::new()))
        })
        .unwrap();
    assert_eq!(
        store.current_serial().unwrap(),
        0,
        "a publication without a replication entry advances no serial"
    );
    assert_eq!(
        store.reference_revision().unwrap(),
        1,
        "the same publication advances the reference revision"
    );

    assert_eq!(
        store
            .compare_and_arm_reclaim_guards(&["kept"], 0, 5, guard(10))
            .unwrap(),
        ReclaimGuardArm::ReferencesMoved
    );
    assert_eq!(store.reclaim_guard("kept").unwrap(), None);

    assert_eq!(
        store
            .compare_and_arm_reclaim_guards(&["orphan"], 1, 5, guard(10))
            .unwrap(),
        ReclaimGuardArm::Armed(vec!["orphan".to_owned()])
    );
    assert_eq!(store.reclaim_guard("orphan").unwrap(), Some(guard(10)));
}

#[test]
fn test_blob_reclaim_guard_blocks_a_reference_that_appends_no_journal_entry() {
    let (_dir, store, now) = stepped_store();
    now.store(10, Ordering::Relaxed);
    let digest = "orphaned-blob-digest";
    store
        .compare_and_arm_reclaim_guards(&[digest], 0, 10, guard(11))
        .unwrap();

    let error = store
        .commit_driver_txn(|txn| {
            txn.put("ref/1", b"points-here")?;
            txn.reference_blob(digest, 6);
            Ok::<_, MetaError>(((), Vec::new()))
        })
        .unwrap_err();

    assert!(matches!(error, MetaError::BlobReclaiming { digest: rejected } if rejected == digest));
    assert!(
        store.get_driver_value("ref/1").unwrap().is_none(),
        "the rejected commit published nothing"
    );
    assert_eq!(
        store.reference_revision().unwrap(),
        0,
        "the rejected commit advanced no reference revision"
    );
}

#[test]
fn test_expired_guard_can_be_reclaimed_without_releasing_a_new_owner() {
    let (_dir, store) = super::store();
    assert_eq!(
        store
            .compare_and_arm_reclaim_guards(&["orphan"], 0, 5, guard(10))
            .unwrap(),
        ReclaimGuardArm::Armed(vec!["orphan".to_owned()])
    );
    assert_eq!(
        store
            .compare_and_arm_reclaim_guards(&["orphan"], 0, 9, guard(20))
            .unwrap(),
        ReclaimGuardArm::Armed(Vec::new())
    );
    assert_eq!(
        store
            .compare_and_arm_reclaim_guards(&["orphan"], 0, 10, guard(20))
            .unwrap(),
        ReclaimGuardArm::Armed(vec!["orphan".to_owned()])
    );
    assert!(!store.compare_and_disarm_reclaim_guard("orphan", guard(10)).unwrap());
    assert!(store.compare_and_disarm_reclaim_guard("orphan", guard(20)).unwrap());
}

#[test]
fn test_disarm_blob_reclaim_guard_reports_an_absent_guard() {
    let (_dir, store) = super::store();
    assert!(!store.compare_and_disarm_reclaim_guard("never-armed", guard(1)).unwrap());
    assert!(store.reclaim_guards().unwrap().is_empty());
}

#[test]
fn test_reclaim_guard_trait_reports_armed_guards() {
    let (_dir, store) = super::store();
    store
        .compare_and_arm_reclaim_guards(&["digest"], 0, 1, guard(2))
        .unwrap();

    assert_eq!(store.reclaim_guards().unwrap(), vec![("digest".to_owned(), guard(2))]);
}

#[test]
fn test_arm_reclaim_guards_accepts_an_empty_batch() {
    let (_dir, store) = super::store();

    assert_eq!(
        store.compare_and_arm_reclaim_guards(&[], 0, 1, guard(2)).unwrap(),
        ReclaimGuardArm::Armed(Vec::new())
    );
    assert!(store.reclaim_guards().unwrap().is_empty());
}

#[test]
fn test_replica_apply_bypasses_a_reclaim_guard() {
    let (_dir, store) = super::store();
    assert_eq!(
        store
            .compare_and_arm_reclaim_guards(&["mirror"], 0, 0, guard(1))
            .unwrap(),
        ReclaimGuardArm::Armed(vec!["mirror".to_owned()])
    );

    // Replicas trust the primary's decision because the primary fenced it before journaling.
    store
        .commit_replica_txn(0, |txn| {
            txn.put("mirror/ref", b"v")?;
            txn.reference_blob("mirror", 4);
            Ok::<_, MetaError>((
                (),
                vec![JournalEntry {
                    payload: b"{}".to_vec(),
                    mutations: vec![DriverMutation::Put {
                        key: "mirror/ref".to_owned(),
                        value: b"v".to_vec(),
                    }],
                    blobs: vec![DriverBlobReference {
                        sha256: "mirror".to_owned(),
                        size: 4,
                    }],
                }],
            ))
        })
        .unwrap();
    assert_eq!(
        store.get_driver_value("mirror/ref").unwrap().as_deref(),
        Some(b"v".as_slice())
    );
}
