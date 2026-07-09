use std::error::Error as _;

use super::{collect_entries, store};
use crate::blob::{BlobError, BlobStore, Digest};

#[test]
fn test_digest_of_known_vector() {
    // sha256("hello")
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
    assert!(matches!(err, BlobError::DigestMismatch { .. }));
}

#[test]
fn test_read_missing_is_not_found() {
    let (_dir, store) = store();
    let err = store.read(&Digest::of(b"absent")).unwrap_err();
    assert!(matches!(err, BlobError::NotFound(_)));
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
    for path in [
        dir.path().join("sha256/zz"),
        dir.path().join("sha256/aa/bb/not-a-digest"),
    ] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"x").unwrap();
    }
    let mut entries = collect_entries(&store);
    entries.sort_by_key(|(_, bytes)| *bytes);
    assert_eq!(entries, vec![(None, 1), (None, 1)]);
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
    assert!(matches!(err, BlobError::NotFound(_)));
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
fn test_write_io_error_when_root_is_a_file() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("not-a-dir");
    std::fs::write(&file, b"x").unwrap();
    let store = BlobStore::new(&file);
    assert!(matches!(store.write(b"data"), Err(BlobError::Io(_))));
}

#[test]
fn test_streamed_blob_commits_after_verification() {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path().join("blobs"));
    let digest = Digest::of(b"streamed content");
    let mut pending = store.begin().unwrap();
    pending.write(b"streamed ").unwrap();
    pending.write(b"content").unwrap();
    store.commit(pending, &digest).unwrap();
    assert_eq!(store.read(&digest).unwrap(), b"streamed content");
}

#[test]
fn test_staged_blob_reports_digest_and_length() {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path().join("blobs"));
    let mut pending = store.begin().unwrap();
    pending.write(b"staged").unwrap();
    let staged = pending.finish().unwrap();
    assert_eq!(
        (staged.digest(), staged.len(), staged.is_empty()),
        (&Digest::of(b"staged"), 6, false)
    );
    store.commit_staged(staged).unwrap();
    assert_eq!(store.read(&Digest::of(b"staged")).unwrap(), b"staged");
}

#[test]
fn test_streamed_blob_with_wrong_digest_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path().join("blobs"));
    let digest = Digest::of(b"expected");
    let mut pending = store.begin().unwrap();
    pending.write(b"tampered").unwrap();
    let err = store.commit(pending, &digest).unwrap_err();
    assert!(matches!(err, BlobError::DigestMismatch { .. }));
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
    assert!(matches!(err, BlobError::Io(_)));
}
