use crate::meta::{MetaError, MetaStore};

#[test]
fn test_open_existing_requires_database_file() {
    let dir = tempfile::tempdir().unwrap();
    assert!(MetaStore::open_existing(dir.path().join("missing.redb")).is_err());
    assert!(MetaStore::open_existing_read_only(dir.path().join("missing.redb")).is_err());
}

#[test]
fn test_open_existing_read_only_reads_and_rejects_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let writable = MetaStore::open(&path).unwrap();
    assert!(format!("{writable:?}").contains("ReadWrite"));
    assert_eq!(writable.next_serial().unwrap(), 1);
    writable.analytics().save(b"snapshot").unwrap();
    drop(writable);

    let read_only = MetaStore::open_existing_read_only(path).unwrap();
    assert!(format!("{read_only:?}").contains("ReadOnly"));
    let analytics = read_only.analytics();

    assert_eq!(read_only.current_serial().unwrap(), 1);
    assert_eq!(analytics.load().unwrap(), Some(b"snapshot".to_vec()));
    assert_read_only(read_only.next_serial().unwrap_err());
    assert_read_only(analytics.save(b"changed").unwrap_err());
    drop(read_only);
    assert_eq!(analytics.load().unwrap(), None);
}

fn assert_read_only(err: MetaError) {
    assert!(matches!(
        err,
        MetaError::Transaction(redb::TransactionError::Storage(redb::StorageError::Io(err)))
            if err.kind() == std::io::ErrorKind::PermissionDenied && err.to_string() == "metadata store is read-only"
    ));
}
