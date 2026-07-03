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
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path());
    let digest = Digest::of(b"payload");
    assert!(!store.exists(&digest));
    let written = store.write(b"payload").unwrap();
    assert_eq!(written, digest);
    assert!(store.exists(&digest));
    assert_eq!(store.read(&digest).unwrap(), b"payload");
}

#[test]
fn test_write_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path());
    let first = store.write(b"same").unwrap();
    let second = store.write(b"same").unwrap();
    assert_eq!(first, second);
}

#[test]
fn test_write_verified_ok() {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path());
    let digest = Digest::of(b"verified");
    store.write_verified(b"verified", &digest).unwrap();
    assert!(store.exists(&digest));
}

#[test]
fn test_write_verified_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path());
    let wrong = Digest::of(b"other");
    let err = store.write_verified(b"verified", &wrong).unwrap_err();
    assert!(matches!(err, BlobError::DigestMismatch { .. }));
}

#[test]
fn test_read_missing_is_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path());
    let err = store.read(&Digest::of(b"absent")).unwrap_err();
    assert!(matches!(err, BlobError::NotFound(_)));
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
