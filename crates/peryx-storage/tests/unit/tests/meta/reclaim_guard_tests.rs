use peryx_ha::{ReclaimGuard, ReclaimGuardArm, ReclaimGuardStore as _};

use crate::meta::MetaError;

fn guard(expires_at_unix: i64) -> ReclaimGuard {
    ReclaimGuard { expires_at_unix }
}

#[test]
fn test_blob_reclaim_guard_blocks_a_reference_and_admits_it_after_deletion() {
    let (_dir, store) = super::store();
    let digest = "orphaned-blob-digest";
    assert_eq!(
        store
            .compare_and_arm_reclaim_guards(&[digest], 0, 10, guard(11))
            .unwrap(),
        ReclaimGuardArm::Armed(vec![digest.to_owned()])
    );
    assert_eq!(store.reclaim_guard(digest).unwrap(), Some(guard(11)));

    let error = store
        .commit_driver_txn(|txn| {
            txn.put("ref/1", b"points-here")?;
            txn.reference_blob(digest, 6);
            Ok::<_, MetaError>(((), vec![b"{}".to_vec()]))
        })
        .unwrap_err();
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
    store
        .commit_driver_txn(|txn| {
            txn.put("ref/1", b"points-here")?;
            txn.reference_blob(digest, 6);
            Ok::<_, MetaError>(((), vec![b"{}".to_vec()]))
        })
        .unwrap();
    assert_eq!(
        store.get_driver_value("ref/1").unwrap().as_deref(),
        Some(b"points-here".as_slice())
    );
}

#[test]
fn test_arm_blob_reclaim_guards_fences_on_an_advanced_serial() {
    let (_dir, store) = super::store();
    store
        .commit_driver_txn(|txn| {
            txn.reference_blob("kept", 1);
            Ok::<_, MetaError>(((), vec![b"{}".to_vec()]))
        })
        .unwrap();
    assert_eq!(
        store.current_serial().unwrap(),
        1,
        "a reference publication advances the serial"
    );

    assert_eq!(
        store
            .compare_and_arm_reclaim_guards(&["kept"], 0, 5, guard(10))
            .unwrap(),
        ReclaimGuardArm::SerialChanged
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
fn test_reclaim_guard_trait_reports_serial_and_armed_guards() {
    let (_dir, store) = super::store();
    assert_eq!(store.reclaim_guard_serial().unwrap(), 0);
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
            Ok::<_, MetaError>(((), vec![b"{}".to_vec()]))
        })
        .unwrap();
    assert_eq!(
        store.get_driver_value("mirror/ref").unwrap().as_deref(),
        Some(b"v".as_slice())
    );
}
