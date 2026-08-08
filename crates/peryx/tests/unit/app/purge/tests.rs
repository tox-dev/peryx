use super::*;

fn meta(dir: &std::path::Path) -> MetaStore {
    MetaStore::open(dir.join("peryx.redb")).unwrap()
}

#[test]
fn test_arm_orphans_arms_every_unreferenced_candidate() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let orphan = blobs.blocking().put_bytes(b"orphan").unwrap();
    let meta = meta(dir.path());
    let candidates = orphan_candidates(&blobs, &BTreeSet::new()).unwrap();

    let armed = arm_orphans(&meta, &candidates, 7, || Ok(BTreeSet::new())).unwrap();

    assert_eq!(armed, vec![0]);
    assert!(meta.blob_reclaim_guarded(orphan.as_str()).unwrap());
}

#[test]
fn test_arm_orphans_retries_when_a_reference_races_the_scan() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let orphan = blobs.blocking().put_bytes(b"orphan").unwrap();
    let raced = blobs.blocking().put_bytes(b"raced").unwrap();
    let meta = meta(dir.path());
    let candidates = orphan_candidates(&blobs, &BTreeSet::new()).unwrap();
    let raced_digest = raced.as_str().to_owned();

    let mut calls = 0;
    let armed = arm_orphans(&meta, &candidates, 0, || {
        calls += 1;
        if calls == 1 {
            // A writer publishes a reference to `raced`, advancing the serial and fencing the arm
            // that scanned before it. The retry rescans and spares the now-referenced blob.
            meta.commit_driver_txn(|txn| {
                txn.reference_blob(&raced_digest, 5);
                Ok::<_, peryx_storage::meta::MetaError>(((), vec![b"{}".to_vec()]))
            })
            .unwrap();
            Ok(BTreeSet::new())
        } else {
            Ok(BTreeSet::from([raced_digest.clone()]))
        }
    })
    .unwrap();

    assert_eq!(calls, 2, "the fenced first attempt forces one rescan");
    let armed_digests: Vec<&str> = armed.iter().map(|&index| candidates[index].digest.as_str()).collect();
    assert_eq!(armed_digests, vec![orphan.as_str()]);
    assert!(meta.blob_reclaim_guarded(orphan.as_str()).unwrap());
    assert!(!meta.blob_reclaim_guarded(raced.as_str()).unwrap());
}

#[test]
fn test_reclaim_guarded_deletes_disarms_and_reports() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let orphan = blobs.blocking().put_bytes(b"orphan").unwrap();
    let meta = meta(dir.path());
    let candidates = orphan_candidates(&blobs, &BTreeSet::new()).unwrap();
    assert!(meta.arm_blob_reclaim_guards(&[orphan.as_str()], 0, 0).unwrap());

    let mut out = Vec::new();
    reclaim_guarded(&blobs, &meta, &candidates, &[0], &mut out).unwrap();

    assert!(blobs.blocking().head(&orphan).unwrap().is_none());
    assert!(!meta.blob_reclaim_guarded(orphan.as_str()).unwrap());
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains(&format!("removed\torphaned-blob\t{}\t6\t", orphan.as_str())));
    assert!(text.contains("summary\tremoved\torphaned-blobs\t1\t6\n"));
}

#[test]
fn test_report_orphans_spares_a_referenced_candidate() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let orphan = blobs.blocking().put_bytes(b"orphan").unwrap();
    let kept = blobs.blocking().put_bytes(b"kept").unwrap();
    let candidates = orphan_candidates(&blobs, &BTreeSet::new()).unwrap();
    let referenced = BTreeSet::from([kept.as_str().to_owned()]);

    let mut out = Vec::new();
    report_orphans(&candidates, &referenced, &mut out).unwrap();

    let text = String::from_utf8(out).unwrap();
    assert!(text.contains(&format!("dry-run\torphaned-blob\t{}\t6\t", orphan.as_str())));
    assert!(!text.contains(kept.as_str()));
    assert!(text.contains("summary\tdry-run\torphaned-blobs\t1\t6\n"));
    assert!(blobs.blocking().head(&orphan).unwrap().is_some());
}

#[test]
fn test_orphan_candidates_reports_a_scan_error() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("blobs");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("sha256"), b"not a directory").unwrap();
    let err = orphan_candidates(&BlobStorage::filesystem(&root), &BTreeSet::new())
        .err()
        .expect("scanning a corrupt store fails");
    assert!(err.to_string().contains("scan orphaned blob files"), "{err}");
}
