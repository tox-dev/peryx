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
