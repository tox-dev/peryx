use crate::meta::MetaError;

#[test]
fn test_blob_reclaim_guard_blocks_a_reference_and_admits_it_after_deletion() {
    let (_dir, store) = super::store();
    let digest = "orphaned-blob-digest";
    // The collector proves the digest unreferenced (serial 0) and arms its guard under the fence.
    assert!(store.arm_blob_reclaim_guards(&[digest], 0, 11).unwrap());
    assert!(store.blob_reclaim_guarded(digest).unwrap());

    // A dedup writer that finds the bytes already present tries to publish only the reference: rejected.
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

    // Deletion finishes, the guard is disarmed, and the writer's retry now commits.
    assert!(store.disarm_blob_reclaim_guard(digest).unwrap());
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

    // Arming against the pre-publication serial is fenced and writes no guard.
    assert!(!store.arm_blob_reclaim_guards(&["kept"], 0, 5).unwrap());
    assert!(!store.blob_reclaim_guarded("kept").unwrap());

    // Arming against the current serial succeeds.
    assert!(store.arm_blob_reclaim_guards(&["orphan"], 1, 5).unwrap());
    assert!(store.blob_reclaim_guarded("orphan").unwrap());
}

#[test]
fn test_clear_blob_reclaim_guards_releases_stranded_guards() {
    let (_dir, store) = super::store();
    assert!(store.arm_blob_reclaim_guards(&["a", "b"], 0, 1).unwrap());

    assert_eq!(store.clear_blob_reclaim_guards().unwrap(), 2);
    assert!(!store.blob_reclaim_guarded("a").unwrap());
    assert!(!store.blob_reclaim_guarded("b").unwrap());
    assert_eq!(
        store.clear_blob_reclaim_guards().unwrap(),
        0,
        "a second clear finds nothing"
    );
}

#[test]
fn test_disarm_blob_reclaim_guard_reports_an_absent_guard() {
    let (_dir, store) = super::store();
    assert!(!store.disarm_blob_reclaim_guard("never-armed").unwrap());
}

#[test]
fn test_replica_apply_bypasses_a_reclaim_guard() {
    let (_dir, store) = super::store();
    assert!(store.arm_blob_reclaim_guards(&["mirror"], 0, 1).unwrap());

    // A replica faithfully applies the primary's already-decided reference even while a local guard is
    // armed; the primary enforced the invariant before it journaled the page.
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
