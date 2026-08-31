use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use peryx_ha::{ReclaimGuard, ReclaimGuardArm, ReclaimGuardStore as _};
use peryx_storage::blob::BlobStorage;
use peryx_storage::meta::{MetaError, MetaStore};

use super::*;

fn stores() -> (tempfile::TempDir, std::path::PathBuf, MetaStore, BlobStorage) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("peryx.redb");
    let meta = MetaStore::open(&path).unwrap();
    let blobs = BlobStorage::filesystem(directory.path().join("blobs"));
    (directory, path, meta, blobs)
}

#[test]
fn dry_run_rechecks_references_without_creating_guard_state() {
    let (_directory, _path, meta, blobs) = stores();
    let orphan = blobs.blocking().put_bytes(b"orphan").unwrap();
    let kept = blobs.blocking().put_bytes(b"kept").unwrap();
    let mut scans = 0;

    let report = purge_orphaned_blobs(&meta, &blobs, false, 10, || {
        scans += 1;
        Ok(if scans == 1 {
            BTreeSet::new()
        } else {
            BTreeSet::from([kept.as_str().to_owned()])
        })
    })
    .unwrap();

    assert_eq!(report.blobs.len(), 1);
    assert_eq!(report.blobs[0].digest, orphan.as_str());
    assert_eq!(report.bytes, 6);
    assert!(blobs.blocking().head(&orphan).unwrap().is_some());
    assert!(blobs.blocking().head(&kept).unwrap().is_some());
    assert!(meta.reclaim_guards().unwrap().is_empty());
}

#[test]
fn confirmed_purge_deletes_and_disarms_owned_candidates() {
    let (_directory, _path, meta, blobs) = stores();
    let orphan = blobs.blocking().put_bytes(b"orphan").unwrap();

    let report = purge_orphaned_blobs(&meta, &blobs, true, 10, || Ok(BTreeSet::new())).unwrap();

    assert_eq!(report.blobs[0].digest, orphan.as_str());
    assert_eq!(report.bytes, 6);
    assert!(blobs.blocking().head(&orphan).unwrap().is_none());
    assert_eq!(meta.reclaim_guard(orphan.as_str()).unwrap(), None);
}

#[test]
fn serial_change_retries_before_arming() {
    let (_directory, _path, meta, blobs) = stores();
    let orphan = blobs.blocking().put_bytes(b"orphan").unwrap();
    let raced = blobs.blocking().put_bytes(b"raced").unwrap();
    let raced_digest = raced.as_str().to_owned();
    let mut scans = 0;

    let report = purge_orphaned_blobs(&meta, &blobs, true, 10, || {
        scans += 1;
        match scans.cmp(&2) {
            std::cmp::Ordering::Equal => {
                meta.commit_driver_txn(|txn| {
                    txn.reference_blob(&raced_digest, 5);
                    Ok::<_, MetaError>(((), vec![b"{}".to_vec()]))
                })
                .unwrap();
                Ok(BTreeSet::new())
            }
            std::cmp::Ordering::Greater => Ok(BTreeSet::from([raced_digest.clone()])),
            std::cmp::Ordering::Less => Ok(BTreeSet::new()),
        }
    })
    .unwrap();

    assert_eq!(scans, 3);
    assert_eq!(report.blobs.len(), 1);
    assert_eq!(report.blobs[0].digest, orphan.as_str());
    assert!(blobs.blocking().head(&raced).unwrap().is_some());
}

#[test]
fn active_owner_blocks_a_second_purge() {
    let (_directory, _path, meta, blobs) = stores();
    let orphan = blobs.blocking().put_bytes(b"orphan").unwrap();
    assert_eq!(
        meta.compare_and_arm_reclaim_guards(&[orphan.as_str()], 0, 10, ReclaimGuard { expires_at_unix: 20 },)
            .unwrap(),
        ReclaimGuardArm::Armed(vec![orphan.as_str().to_owned()])
    );

    let report = purge_orphaned_blobs(&meta, &blobs, true, 11, || Ok(BTreeSet::new())).unwrap();

    assert!(report.blobs.is_empty());
    assert!(blobs.blocking().head(&orphan).unwrap().is_some());
    assert_eq!(
        meta.reclaim_guard(orphan.as_str()).unwrap(),
        Some(ReclaimGuard { expires_at_unix: 20 })
    );
}

#[test]
fn expired_owner_is_replaced_before_deletion() {
    let (_directory, _path, meta, blobs) = stores();
    let orphan = blobs.blocking().put_bytes(b"orphan").unwrap();
    meta.compare_and_arm_reclaim_guards(&[orphan.as_str()], 0, 0, ReclaimGuard { expires_at_unix: 10 })
        .unwrap();

    let report = purge_orphaned_blobs(&meta, &blobs, true, 10, || Ok(BTreeSet::new())).unwrap();

    assert_eq!(report.blobs.len(), 1);
    assert!(blobs.blocking().head(&orphan).unwrap().is_none());
    assert_eq!(meta.reclaim_guard(orphan.as_str()).unwrap(), None);
}

#[test]
fn expired_guard_for_a_present_blob_is_released() {
    let (_directory, _path, meta, blobs) = stores();
    let kept = blobs.blocking().put_bytes(b"kept").unwrap();
    meta.compare_and_arm_reclaim_guards(&[kept.as_str()], 0, 0, ReclaimGuard { expires_at_unix: 10 })
        .unwrap();

    let report = purge_orphaned_blobs(&meta, &blobs, true, 10, || {
        Ok(BTreeSet::from([kept.as_str().to_owned()]))
    })
    .unwrap();

    assert!(report.blobs.is_empty());
    assert!(blobs.blocking().head(&kept).unwrap().is_some());
    assert_eq!(meta.reclaim_guard(kept.as_str()).unwrap(), None);
}

#[test]
fn expired_guard_for_an_absent_blob_is_released() {
    let (_directory, _path, meta, blobs) = stores();
    let digest = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    meta.compare_and_arm_reclaim_guards(&[digest], 0, 0, ReclaimGuard { expires_at_unix: 10 })
        .unwrap();

    let report = purge_orphaned_blobs(&meta, &blobs, true, 10, || Ok(BTreeSet::new())).unwrap();

    assert!(report.blobs.is_empty());
    assert_eq!(meta.reclaim_guard(digest).unwrap(), None);
}

#[test]
fn reference_failure_stops_before_guarding_or_deleting() {
    let (_directory, _path, meta, blobs) = stores();
    let orphan = blobs.blocking().put_bytes(b"orphan").unwrap();

    let error = purge_orphaned_blobs(&meta, &blobs, true, 10, || Err("inventory unavailable".to_owned())).unwrap_err();

    assert_eq!(
        error.to_string(),
        "scan metadata blob references: inventory unavailable"
    );
    assert!(blobs.blocking().head(&orphan).unwrap().is_some());
    assert!(meta.reclaim_guards().unwrap().is_empty());
}

#[test]
fn corrupt_blob_tree_returns_a_scan_error() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("blobs");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("sha256"), b"not a directory").unwrap();
    let meta = MetaStore::open(directory.path().join("peryx.redb")).unwrap();

    let error =
        purge_orphaned_blobs(&meta, &BlobStorage::filesystem(root), false, 10, || Ok(BTreeSet::new())).unwrap_err();

    assert!(error.to_string().starts_with("scan orphaned blob files:"));
}

#[test]
fn deletion_failure_keeps_the_guard_armed() {
    let (_directory, _path, meta, blobs) = stores();
    let orphan = blobs.blocking().put_bytes(b"orphan").unwrap();
    let orphan_path = purge_orphaned_blobs(&meta, &blobs, false, 10, || Ok(BTreeSet::new()))
        .unwrap()
        .blobs
        .pop()
        .unwrap()
        .path;
    let mut scans = 0;

    let error = purge_orphaned_blobs(&meta, &blobs, true, 10, || {
        scans += 1;
        if scans == 2 {
            std::fs::remove_file(&orphan_path).unwrap();
            std::fs::create_dir(&orphan_path).unwrap();
            std::fs::write(orphan_path.join("entry"), b"occupied").unwrap();
        }
        Ok(BTreeSet::new())
    })
    .unwrap_err();

    assert!(matches!(
        error,
        OrphanPurgeError::Blob {
            operation: "delete orphaned blob",
            ..
        }
    ));
    assert!(meta.reclaim_guard(orphan.as_str()).unwrap().is_some());
}

#[test]
fn an_interrupted_purge_stops_rejecting_references_once_its_lease_lapses() {
    let directory = tempfile::tempdir().unwrap();
    let ticks = Arc::new(AtomicI64::new(10));
    let clock = Arc::clone(&ticks);
    let meta = MetaStore::open(directory.path().join("peryx.redb"))
        .unwrap()
        .with_clock(Arc::new(move || clock.load(Ordering::Relaxed)));
    let blobs = BlobStorage::filesystem(directory.path().join("blobs"));
    let orphan = blobs.blocking().put_bytes(b"orphan").unwrap();
    let orphan_path = purge_orphaned_blobs(&meta, &blobs, false, 10, || Ok(BTreeSet::new()))
        .unwrap()
        .blobs
        .pop()
        .unwrap()
        .path;
    let mut scans = 0;
    purge_orphaned_blobs(&meta, &blobs, true, 10, || {
        scans += 1;
        if scans == 2 {
            std::fs::remove_file(&orphan_path).unwrap();
            std::fs::create_dir(&orphan_path).unwrap();
            std::fs::write(orphan_path.join("entry"), b"occupied").unwrap();
        }
        Ok(BTreeSet::new())
    })
    .unwrap_err();
    let lease = ReclaimGuard {
        expires_at_unix: 10 + RECLAIM_GUARD_LEASE_SECS,
    };
    assert_eq!(meta.reclaim_guard(orphan.as_str()).unwrap(), Some(lease));

    ticks.store(lease.expires_at_unix - 1, Ordering::Relaxed);
    let rejected = republish(&meta, orphan.as_str()).unwrap_err();
    ticks.store(lease.expires_at_unix, Ordering::Relaxed);
    republish(&meta, orphan.as_str()).unwrap();

    assert!(matches!(rejected, MetaError::BlobReclaiming { digest } if digest == orphan.as_str()));
    assert_eq!(meta.reclaim_guard(orphan.as_str()).unwrap(), None);
}

fn republish(meta: &MetaStore, digest: &str) -> Result<(), MetaError> {
    meta.commit_driver_txn(|txn| {
        txn.reference_blob(digest, 6);
        Ok::<_, MetaError>(((), vec![b"{}".to_vec()]))
    })
}
