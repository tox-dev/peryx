use std::error::Error as _;

use super::{collect_entries, store};
use crate::blob::{BlobStore, Digest};
use rstest::rstest;

#[test]
fn test_digest_of_known_vector() {
    assert_eq!(
        Digest::of(b"hello").as_str(),
        "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
    );
}

#[test]
fn test_from_hex_accepts_valid_and_rejects_invalid() {
    let valid = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
    assert_eq!(Digest::from_hex(valid).unwrap().as_str(), valid);
    assert!(Digest::from_hex("tooshort").is_none());
    assert!(Digest::from_hex(&"Z".repeat(64)).is_none());
    assert!(Digest::from_hex(&"A".repeat(64)).is_none()); // uppercase rejected
}

#[test]
fn test_from_artifact_digest_carries_the_hex() {
    let content = b"payload";
    let blob = Digest::of(content);
    let artifact = peryx_identity::ArtifactDigest::from_sha256(blob.as_str()).unwrap();
    assert_eq!(Digest::from_hex(artifact.sha256()), Some(blob));
}

#[test]
fn test_path_for_is_sharded() {
    let store = BlobStore::new("/data");
    let digest = Digest::of(b"hello");
    let path = store.path_for(&digest);
    assert!(path.ends_with(format!("sha256/2c/f2/{}", digest.as_str())));
}

#[test]
fn test_write_read_roundtrip_and_exists() {
    let (_dir, store) = store();
    let digest = Digest::of(b"payload");
    assert!(!store.exists(&digest));
    let written = store.write(b"payload").unwrap();
    assert_eq!(written, digest);
    assert!(store.exists(&digest));
    assert_eq!(store.read(&digest).unwrap(), b"payload");
}

#[test]
fn test_write_is_idempotent() {
    let (_dir, store) = store();
    let first = store.write(b"same").unwrap();
    let second = store.write(b"same").unwrap();
    assert_eq!(first, second);
}

#[test]
fn test_write_verified_ok() {
    let (_dir, store) = store();
    let digest = Digest::of(b"verified");
    store.write_verified(b"verified", &digest).unwrap();
    assert!(store.exists(&digest));
}

#[test]
fn test_write_verified_mismatch() {
    let (_dir, store) = store();
    let wrong = Digest::of(b"other");
    let err = store.write_verified(b"verified", &wrong).unwrap_err();
    assert_eq!(err.kind(), crate::blob::BlobErrorKind::DigestMismatch);
}

#[test]
fn test_read_missing_is_not_found() {
    let (_dir, store) = store();
    let err = store.read(&Digest::of(b"absent")).unwrap_err();
    assert_eq!(err.kind(), crate::blob::BlobErrorKind::NotFound);
}

#[test]
fn test_head_reports_length_without_reading() {
    let (_dir, store) = store();
    let digest = store.write(b"payload").unwrap();
    assert_eq!(store.head(&digest).unwrap().unwrap().bytes, 7);
    assert_eq!(store.head(&Digest::of(b"absent")).unwrap(), None);
}

#[test]
fn test_head_treats_a_digest_directory_as_absent() {
    let (_dir, store) = store();
    let digest = Digest::of(b"directory");
    std::fs::create_dir_all(store.path_for(&digest)).unwrap();
    assert_eq!(store.head(&digest).unwrap(), None);
}

#[cfg(unix)]
#[test]
fn test_head_reports_a_filesystem_error() {
    let (_dir, store) = store();
    let digest = Digest::of(b"loop");
    let path = store.path_for(&digest);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&path, &path).unwrap();
    assert_eq!(store.head(&digest).unwrap_err().kind(), crate::blob::BlobErrorKind::Io);
}

#[test]
fn test_read_range_uses_end_exclusive_offsets() {
    let (_dir, store) = store();
    let digest = store.write(b"payload").unwrap();
    assert_eq!(store.read_range(&digest, 1..5).unwrap(), b"aylo");
    assert_eq!(store.read_range(&digest, 7..7).unwrap(), b"");
}

#[test]
fn test_read_range_missing_is_not_found() {
    let (_dir, store) = store();
    let digest = Digest::of(b"missing");
    assert_eq!(
        store.read_range(&digest, 0..1).unwrap_err().kind(),
        crate::blob::BlobErrorKind::NotFound
    );
}

#[test]
fn test_read_range_rejects_out_of_bounds_offsets() {
    let (_dir, store) = store();
    let digest = store.write(b"payload").unwrap();
    assert_eq!(
        store.read_range(&digest, 3..8).unwrap_err().kind(),
        crate::blob::BlobErrorKind::InvalidRange
    );
    let (start, end) = (5, 4);
    assert_eq!(
        store.read_range(&digest, start..end).unwrap_err().kind(),
        crate::blob::BlobErrorKind::InvalidRange
    );
}

#[test]
fn test_health_check_creates_and_reads_the_store_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("blobs");
    let store = BlobStore::new(&root);
    store.health_check().unwrap();
    assert!(root.is_dir());
}

#[test]
fn test_health_check_rejects_a_file_as_the_store_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("not-a-directory");
    std::fs::write(&root, b"x").unwrap();
    let store = BlobStore::new(root);
    assert_eq!(store.health_check().unwrap_err().kind(), crate::blob::BlobErrorKind::Io);
}

#[cfg(unix)]
#[test]
fn test_health_check_ignores_a_missing_lease_target() {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path().join("blobs"));
    let lease_dir = dir.path().join("blobs/.leases");
    std::fs::create_dir_all(&lease_dir).unwrap();
    let missing = lease_dir.join(".peryx-lease-missing");
    std::os::unix::fs::symlink(lease_dir.join("absent"), &missing).unwrap();
    let stale = lease_dir.join(".peryx-lease-stale");
    std::fs::write(&stale, b"stale").unwrap();

    store.health_check().unwrap();

    assert!(std::fs::symlink_metadata(missing).is_ok());
    assert!(!stale.exists());
}

#[cfg(unix)]
#[test]
fn test_health_check_reports_an_unreadable_lease() {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path().join("blobs"));
    let lease = dir.path().join("blobs/.leases/.peryx-lease-loop");
    std::fs::create_dir_all(lease.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&lease, &lease).unwrap();
    assert_eq!(store.health_check().unwrap_err().kind(), crate::blob::BlobErrorKind::Io);
}

#[test]
fn test_scan_reports_blob_entries() {
    let (_dir, store) = store();
    let digest = store.write(b"payload").unwrap();
    assert_eq!(collect_entries(&store), vec![(Some(digest), 7)]);
}

#[test]
fn test_scan_empty_store_reports_no_entries() {
    let (_dir, store) = store();
    assert!(collect_entries(&store).is_empty());
}

#[test]
fn test_scan_marks_invalid_blob_paths() {
    let (dir, store) = store();
    let digest = Digest::of(b"payload");
    let hex = digest.as_str();
    for path in [
        dir.path().join("sha256/short"),
        dir.path().join("sha256/aa/bb/not-a-digest"),
        dir.path().join(format!("sha256/zz/{}/{}", &hex[2..4], hex)),
        dir.path().join(format!("sha256/{}/zz/{}", &hex[..2], hex)),
    ] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"x").unwrap();
    }
    let mut entries = collect_entries(&store);
    entries.sort_by_key(|(_, bytes)| *bytes);
    assert_eq!(entries, vec![(None, 1); 4]);
}

#[cfg(unix)]
#[test]
fn test_scan_skips_symlink_entries() {
    let (dir, store) = store();
    std::fs::create_dir_all(dir.path().join("sha256/aa")).unwrap();
    std::os::unix::fs::symlink(dir.path(), dir.path().join("sha256/aa/link")).unwrap();
    assert!(collect_entries(&store).is_empty());
}

#[test]
fn test_scan_visit_error_reports_source() {
    let (_dir, store) = store();
    store.write(b"payload").unwrap();
    let err = store.scan(|_entry| Err(std::io::Error::other("stop"))).unwrap_err();
    assert_eq!(err.to_string(), "stop");
    assert!(err.source().is_some());
}

#[test]
fn test_verify_streams_blob_hash_check() {
    let (_dir, store) = store();
    let digest = store.write(b"payload").unwrap();
    assert!(store.verify(&digest).unwrap());
}

#[test]
fn test_verify_detects_digest_mismatch() {
    let (_dir, store) = store();
    let digest = Digest::of(b"expected");
    let path = store.path_for(&digest);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, b"tampered").unwrap();
    assert!(!store.verify(&digest).unwrap());
}

#[test]
fn test_verify_missing_is_not_found() {
    let (_dir, store) = store();
    let err = store.verify(&Digest::of(b"absent")).unwrap_err();
    assert_eq!(err.kind(), crate::blob::BlobErrorKind::NotFound);
}

#[test]
fn test_remove_deletes_existing_blob() {
    let (_dir, store) = store();
    let digest = store.write(b"payload").unwrap();
    assert!(store.remove(&digest).unwrap());
    assert!(!store.exists(&digest));
    assert!(!store.remove(&digest).unwrap());
}

#[test]
fn test_remove_prunes_empty_fan_out_directories() {
    let (_dir, store) = store();
    let digest = store.write(b"payload").unwrap();
    let cd = store.path_for(&digest).parent().unwrap().to_path_buf();
    let ab = cd.parent().unwrap().to_path_buf();
    assert!(cd.is_dir());
    store.remove(&digest).unwrap();
    assert!(!cd.exists());
    assert!(!ab.exists());
}

#[test]
fn test_remove_stops_pruning_at_a_directory_a_sibling_occupies() {
    let (_dir, store) = store();
    let digest = store.write(b"payload").unwrap();
    let cd = store.path_for(&digest).parent().unwrap().to_path_buf();
    let ab = cd.parent().unwrap().to_path_buf();
    let sibling = ab.join("zz");
    std::fs::create_dir_all(&sibling).unwrap();
    std::fs::write(sibling.join("other"), b"x").unwrap();
    store.remove(&digest).unwrap();
    assert!(!cd.exists(), "the emptied second-level directory is pruned");
    assert!(ab.exists(), "the first-level directory a sibling still occupies stays");
}

#[test]
fn test_remove_io_error_when_blob_path_is_a_directory() {
    let (_dir, store) = store();
    let digest = Digest::of(b"payload");
    let path = store.path_for(&digest);
    std::fs::create_dir_all(&path).unwrap();
    assert_eq!(
        store.remove(&digest).unwrap_err().kind(),
        crate::blob::BlobErrorKind::Io
    );
}

#[test]
fn test_write_io_error_when_root_is_a_file() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("not-a-dir");
    std::fs::write(&file, b"x").unwrap();
    let store = BlobStore::new(&file);
    assert_eq!(store.write(b"data").unwrap_err().kind(), crate::blob::BlobErrorKind::Io);
}

#[test]
fn test_streamed_blob_commits_after_verification() {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path().join("blobs"));
    let digest = Digest::of(b"streamed content");
    let mut pending = store.begin().unwrap();
    assert_eq!((pending.len(), pending.is_empty()), (0, true));
    pending.write(b"streamed ").unwrap();
    assert_eq!((pending.len(), pending.is_empty()), (9, false));
    pending.write(b"content").unwrap();
    store.commit(pending, &digest).unwrap();
    assert_eq!(store.read(&digest).unwrap(), b"streamed content");
}

#[test]
fn test_commit_returns_a_filesystem_placement_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path().join("blobs"));
    let digest = Digest::of(b"receipt content");
    let mut pending = store.begin().unwrap();
    pending.write(b"receipt content").unwrap();

    let receipt = store.commit(pending, &digest).unwrap();

    assert_eq!(receipt.digest, digest);
    assert_eq!(receipt.size, 15);
    assert_eq!(receipt.durability, crate::blob::DurabilityCapabilities::FILESYSTEM);
}

#[test]
fn test_committing_an_already_present_blob_still_returns_a_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path().join("blobs"));
    let digest = store.write(b"present").unwrap();
    let mut pending = store.begin().unwrap();
    pending.write(b"present").unwrap();
    let staged = pending.finish().unwrap();

    let receipt = store.commit_staged(staged).unwrap();

    assert_eq!(receipt.digest, digest);
    assert_eq!(receipt.size, 7);
    assert_eq!(receipt.durability, crate::blob::DurabilityCapabilities::FILESYSTEM);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn test_materialized_lease_copies_across_filesystems() {
    #[cfg(target_os = "linux")]
    let source_root = tempfile::tempdir().unwrap();
    #[cfg(target_os = "linux")]
    let lease_root = tempfile::tempdir_in("/dev/shm").unwrap();
    #[cfg(target_os = "macos")]
    let source_root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    #[cfg(target_os = "macos")]
    let lease_root = tempfile::tempdir().unwrap();
    let source = source_root.path().join("blob");
    std::fs::write(&source, b"payload").unwrap();

    let lease = crate::blob::BlobLease::pinned(&source, lease_root.path()).unwrap();

    assert_eq!(std::fs::read(lease.path()).unwrap(), b"payload");
}

#[test]
fn test_streamed_blob_with_wrong_digest_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path().join("blobs"));
    let digest = Digest::of(b"expected");
    let mut pending = store.begin().unwrap();
    pending.write(b"tampered").unwrap();
    let err = store.commit(pending, &digest).unwrap_err();
    assert_eq!(err.kind(), crate::blob::BlobErrorKind::DigestMismatch);
    assert!(!store.exists(&digest));
}

#[cfg(unix)]
#[test]
fn test_commit_into_an_unwritable_store_is_an_io_error() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path().join("blobs"));
    let digest = Digest::of(b"blocked");
    let mut pending = store.begin().unwrap();
    pending.write(b"blocked").unwrap();
    let parent = store.path_for(&digest).parent().unwrap().to_path_buf();
    std::fs::create_dir_all(&parent).unwrap();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o555)).unwrap();
    let err = store.commit(pending, &digest).unwrap_err();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(err.kind(), crate::blob::BlobErrorKind::Io);
}

#[test]
fn test_stage_upload_chunk_accumulates_and_reports_length() {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path().join("blobs"));

    assert_eq!(store.stage_upload_chunk("sess-1", 0, b"hello ").unwrap(), 6);
    assert_eq!(store.stage_upload_chunk("sess-1", 6, b"world").unwrap(), 11);
    assert_eq!(store.staged_upload_len("sess-1").unwrap(), Some(11));
}

#[rstest]
#[case::empty(b"", 5)]
#[case::nonempty(b"hello", 8)]
fn test_stage_upload_chunk_rejects_gaps_without_changing_bytes(#[case] initial: &[u8], #[case] offset: u64) {
    let (_dir, store) = store();
    store.stage_upload_chunk("sess-1", 0, initial).unwrap();

    let error = store.stage_upload_chunk("sess-1", offset, b"x").unwrap_err();

    assert_eq!(
        (error.kind(), error.invalid_range_values()),
        (
            crate::blob::BlobErrorKind::InvalidRange,
            Some((offset, offset, initial.len() as u64))
        )
    );
    let digest = Digest::of(initial);
    store.finish_upload("sess-1", &digest).unwrap();
    assert_eq!(store.read(&digest).unwrap(), initial);
}

#[test]
fn test_stage_upload_chunk_resumes_idempotently_past_a_lost_offset() {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path().join("blobs"));

    assert_eq!(store.stage_upload_chunk("sess-1", 0, b"hello ").unwrap(), 6);
    assert_eq!(store.stage_upload_chunk("sess-1", 6, b"WORLD").unwrap(), 11);
    assert_eq!(store.stage_upload_chunk("sess-1", 6, b"world").unwrap(), 11);
    let digest = Digest::of(b"hello world");

    store.finish_upload("sess-1", &digest).unwrap();

    assert_eq!(store.read(&digest).unwrap(), b"hello world");
}

#[test]
fn test_stage_upload_chunk_surfaces_an_unusable_session_name_as_io() {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path().join("blobs"));
    // An interior NUL isolates invalid input from the fresh-stage and resumed-stage paths.
    let error = store.stage_upload_chunk("bad\0session", 0, b"x").unwrap_err();

    assert_eq!(error.kind(), crate::blob::BlobErrorKind::Io);
}

#[test]
fn test_staged_upload_len_is_none_for_an_unknown_session() {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path().join("blobs"));

    assert_eq!(store.staged_upload_len("ghost").unwrap(), None);
}

#[test]
fn test_finish_upload_publishes_the_blob_and_clears_the_stage() {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path().join("blobs"));
    store.stage_upload_chunk("sess-1", 0, b"streamed ").unwrap();
    store.stage_upload_chunk("sess-1", 9, b"content").unwrap();
    let digest = Digest::of(b"streamed content");

    store.finish_upload("sess-1", &digest).unwrap();

    assert_eq!(store.read(&digest).unwrap(), b"streamed content");
    assert_eq!(store.staged_upload_len("sess-1").unwrap(), None);
}

#[test]
fn test_finish_upload_rejects_a_digest_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path().join("blobs"));
    store.stage_upload_chunk("sess-1", 0, b"tampered").unwrap();
    let expected = Digest::of(b"expected");

    let error = store.finish_upload("sess-1", &expected).unwrap_err();

    assert_eq!(error.kind(), crate::blob::BlobErrorKind::DigestMismatch);
    assert_eq!(store.staged_upload_len("sess-1").unwrap(), Some(8));
    assert!(!store.exists(&expected));
}

#[test]
fn test_finish_upload_on_an_absent_stage_is_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path().join("blobs"));

    let error = store.finish_upload("ghost", &Digest::of(b"x")).unwrap_err();

    assert_eq!(error.kind(), crate::blob::BlobErrorKind::NotFound);
}

#[test]
fn test_finish_upload_deduplicates_an_already_stored_blob() {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path().join("blobs"));
    let digest = store.write(b"present").unwrap();
    store.stage_upload_chunk("sess-1", 0, b"present").unwrap();

    store.finish_upload("sess-1", &digest).unwrap();

    assert_eq!(store.read(&digest).unwrap(), b"present");
    assert_eq!(store.staged_upload_len("sess-1").unwrap(), None);
}

#[test]
fn test_finish_upload_is_idempotent_after_a_successful_finish() {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path().join("blobs"));
    store.stage_upload_chunk("sess-1", 0, b"streamed content").unwrap();
    let digest = Digest::of(b"streamed content");
    store.finish_upload("sess-1", &digest).unwrap();

    store.finish_upload("sess-1", &digest).unwrap();
    assert_eq!(store.read(&digest).unwrap(), b"streamed content");
}

#[test]
fn test_finish_upload_surfaces_an_unreadable_stage_as_io() {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path().join("blobs"));
    // An interior NUL isolates invalid input from absent-stage and published-digest paths.
    let error = store.finish_upload("bad\0session", &Digest::of(b"x")).unwrap_err();
    assert_eq!(error.kind(), crate::blob::BlobErrorKind::Io);
}

#[test]
fn test_discard_upload_removes_the_stage_and_tolerates_absence() {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path().join("blobs"));
    store.stage_upload_chunk("sess-1", 0, b"partial").unwrap();

    store.discard_upload("sess-1").unwrap();
    assert_eq!(store.staged_upload_len("sess-1").unwrap(), None);
    store.discard_upload("sess-1").unwrap();
}

#[test]
fn test_staged_upload_len_surfaces_an_unreadable_stage_as_io() {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path().join("blobs"));
    // An interior NUL isolates invalid input from the absent-stage path.
    let error = store.staged_upload_len("bad\0session").unwrap_err();

    assert_eq!(error.kind(), crate::blob::BlobErrorKind::Io);
}

#[test]
fn test_discard_upload_surfaces_a_filesystem_fault() {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path().join("blobs"));
    // An interior NUL isolates invalid input from an already-absent stage.
    let error = store.discard_upload("bad\0session").unwrap_err();

    assert_eq!(error.kind(), crate::blob::BlobErrorKind::Io);
}

#[derive(Clone, Copy)]
enum RepairOperation {
    Write,
    CommitStaged,
    FinishUpload,
}

#[derive(Clone, Copy)]
enum Corruption {
    Truncated,
    SameLength,
}

#[rstest]
#[case::write_truncated(RepairOperation::Write, Corruption::Truncated)]
#[case::write_same_length(RepairOperation::Write, Corruption::SameLength)]
#[case::commit_truncated(RepairOperation::CommitStaged, Corruption::Truncated)]
#[case::commit_same_length(RepairOperation::CommitStaged, Corruption::SameLength)]
#[case::finish_truncated(RepairOperation::FinishUpload, Corruption::Truncated)]
#[case::finish_same_length(RepairOperation::FinishUpload, Corruption::SameLength)]
fn test_operation_repairs_resident_blob(#[case] operation: RepairOperation, #[case] corruption: Corruption) {
    const CONTENT: &[u8] = b"streamed content";
    let (_dir, store) = store();
    let digest = store.write(CONTENT).unwrap();
    std::fs::write(
        store.path_for(&digest),
        match corruption {
            Corruption::Truncated => &CONTENT[..6],
            Corruption::SameLength => b"STREAMED CONTENT",
        },
    )
    .unwrap();

    match operation {
        RepairOperation::Write => {
            assert_eq!(store.write(CONTENT).unwrap(), digest);
        }
        RepairOperation::CommitStaged => {
            let mut pending = store.begin().unwrap();
            pending.write(CONTENT).unwrap();
            store.commit_staged(pending.finish().unwrap()).unwrap();
        }
        RepairOperation::FinishUpload => {
            store.stage_upload_chunk("sess-1", 0, CONTENT).unwrap();
            store.finish_upload("sess-1", &digest).unwrap();
            assert_eq!(store.staged_upload_len("sess-1").unwrap(), None);
        }
    }

    assert_eq!(store.read(&digest).unwrap(), CONTENT);
    assert!(store.verify(&digest).unwrap());
}

#[test]
fn test_write_leaves_a_valid_resident_blob_untouched() {
    let (_dir, store) = store();
    let digest = store.write(b"payload").unwrap();
    let before = std::fs::metadata(store.path_for(&digest)).unwrap().modified().unwrap();

    store.write(b"payload").unwrap();

    let after = std::fs::metadata(store.path_for(&digest)).unwrap().modified().unwrap();
    assert_eq!(before, after);
}

#[test]
fn test_commit_over_a_directory_at_the_digest_path_is_an_io_error() {
    let (_dir, store) = store();
    let digest = Digest::of(b"blocked");
    // A directory occupant distinguishes a move failure from a lost writer race.
    std::fs::create_dir_all(store.path_for(&digest)).unwrap();
    let mut pending = store.begin().unwrap();
    pending.write(b"blocked").unwrap();

    let error = store.commit_staged(pending.finish().unwrap()).unwrap_err();

    assert_eq!(error.kind(), crate::blob::BlobErrorKind::Io);
}

#[test]
fn test_concurrent_commits_repair_a_corrupt_destination() {
    let (_dir, store) = store();
    let digest = Digest::of(b"contended");
    // Same-length corruption prevents either writer from taking the deduplication path.
    let dest = store.path_for(&digest);
    std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
    std::fs::write(&dest, b"CONTENDED").unwrap();

    let handles: Vec<_> = (0..2)
        .map(|_| {
            let store = store.clone();
            let digest = digest.clone();
            std::thread::spawn(move || {
                let mut pending = store.begin().unwrap();
                pending.write(b"contended").unwrap();
                store.commit_staged(pending.finish().unwrap()).unwrap();
                assert!(store.verify(&digest).unwrap());
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }

    assert_eq!(store.read(&digest).unwrap(), b"contended");
}

/// Writable and traversable but not readable: a rename into `directory` lands, and the flush that would
/// make it durable is refused.
#[cfg(unix)]
fn while_unflushable<T>(directory: &std::path::Path, act: impl FnOnce() -> T) -> T {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o333)).unwrap();
    let outcome = act();
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o755)).unwrap();
    outcome
}

#[cfg(unix)]
#[test]
fn test_commit_issues_no_receipt_when_the_digest_directory_cannot_be_flushed() {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path().join("blobs"));
    let digest = Digest::of(b"unflushed");
    let parent = store.path_for(&digest).parent().unwrap().to_path_buf();
    std::fs::create_dir_all(&parent).unwrap();
    let mut pending = store.begin().unwrap();
    pending.write(b"unflushed").unwrap();

    let committed = while_unflushable(&parent, || store.commit(pending, &digest));

    assert_eq!(committed.unwrap_err().kind(), crate::blob::BlobErrorKind::Io);
}

#[cfg(unix)]
#[test]
fn test_commit_issues_no_receipt_when_a_new_fan_out_ancestor_cannot_be_flushed() {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path().join("blobs"));
    let digest = Digest::of(b"fresh fan-out");
    let mut pending = store.begin().unwrap();
    pending.write(b"fresh fan-out").unwrap();

    let committed = while_unflushable(&store.staging_dir(), || store.commit(pending, &digest));

    assert_eq!(committed.unwrap_err().kind(), crate::blob::BlobErrorKind::Io);
}

#[cfg(unix)]
#[test]
fn test_commit_over_a_matching_resident_issues_no_receipt_when_the_flush_fails() {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path().join("blobs"));
    let digest = store.write(b"resident").unwrap();
    let parent = store.path_for(&digest).parent().unwrap().to_path_buf();
    let mut pending = store.begin().unwrap();
    pending.write(b"resident").unwrap();

    let committed = while_unflushable(&parent, || store.commit(pending, &digest));

    assert_eq!(committed.unwrap_err().kind(), crate::blob::BlobErrorKind::Io);
    assert_eq!(store.read(&digest).unwrap(), b"resident");
}

#[cfg(unix)]
#[test]
fn test_finish_upload_retry_reports_an_unflushable_published_parent() {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path().join("blobs"));
    let digest = Digest::of(b"resumed");
    store.stage_upload_chunk("sess-1", 0, b"resumed").unwrap();
    store.finish_upload("sess-1", &digest).unwrap();
    let parent = store.path_for(&digest).parent().unwrap().to_path_buf();

    let retried = while_unflushable(&parent, || store.finish_upload("sess-1", &digest));

    assert_eq!(retried.unwrap_err().kind(), crate::blob::BlobErrorKind::Io);
}

#[cfg(unix)]
#[test]
fn test_stage_upload_chunk_accepts_no_bytes_when_the_stage_entry_cannot_be_flushed() {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path().join("blobs"));
    let uploads = dir.path().join("blobs/uploads");
    std::fs::create_dir_all(&uploads).unwrap();

    let staged = while_unflushable(&uploads, || store.stage_upload_chunk("sess-1", 0, b"hello"));

    assert_eq!(staged.unwrap_err().kind(), crate::blob::BlobErrorKind::Io);
}

/// The entry is durable from the call that created it, so a resume must not pay for a directory flush.
#[cfg(unix)]
#[test]
fn test_stage_upload_chunk_resumes_without_reflushing_the_stage_directory() {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path().join("blobs"));
    let uploads = dir.path().join("blobs/uploads");
    store.stage_upload_chunk("sess-1", 0, b"hello ").unwrap();

    let resumed = while_unflushable(&uploads, || store.stage_upload_chunk("sess-1", 6, b"world"));

    assert_eq!(resumed.unwrap(), 11);
}

#[cfg(unix)]
#[test]
fn test_write_of_a_matching_resident_reports_an_unflushable_parent() {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path().join("blobs"));
    let digest = store.write(b"rewritten").unwrap();
    let parent = store.path_for(&digest).parent().unwrap().to_path_buf();

    let written = while_unflushable(&parent, || store.write(b"rewritten"));

    assert_eq!(written.unwrap_err().kind(), crate::blob::BlobErrorKind::Io);
    assert_eq!(store.read(&digest).unwrap(), b"rewritten");
}
