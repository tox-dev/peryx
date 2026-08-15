use redb::TableDefinition;

use super::store;
use crate::meta::{MetaError, MetaStore, WriterIdentityError};

#[test]
fn test_writer_identity_claim_persists_and_repeats() {
    let (dir, store) = store();
    assert_eq!(store.writer_identity().unwrap(), None);

    store.claim_writer_identity("writer-a").unwrap();
    store.claim_writer_identity("writer-a").unwrap();
    drop(store);

    let reopened = MetaStore::open_existing(dir.path().join("peryx.redb")).unwrap();
    assert_eq!(reopened.writer_identity().unwrap().as_deref(), Some("writer-a"));
}

#[test]
fn test_writer_identity_claim_rejects_another_writer() {
    let (_dir, store) = store();
    store.claim_writer_identity("writer-a").unwrap();

    let error = store.claim_writer_identity("writer-b").unwrap_err();

    assert_eq!(
        error.to_string(),
        "metadata store is claimed by writer \"writer-a\"; refusing \"writer-b\""
    );
    assert!(matches!(
        error,
        WriterIdentityError::Claimed { active, requested }
            if active == "writer-a" && requested == "writer-b"
    ));
    assert_eq!(store.writer_identity().unwrap().as_deref(), Some("writer-a"));
}

#[test]
fn test_writer_identity_promotion_requires_the_active_writer() {
    let (_dir, store) = store();
    store.claim_writer_identity("writer-a").unwrap();

    let error = store.promote_writer_identity("stale", "writer-b").unwrap_err();

    assert_eq!(
        error.to_string(),
        "metadata store writer is Some(\"writer-a\"); expected \"stale\""
    );
    assert!(matches!(
        error,
        WriterIdentityError::Changed { active, expected }
            if active.as_deref() == Some("writer-a") && expected == "stale"
    ));
    assert_eq!(store.writer_identity().unwrap().as_deref(), Some("writer-a"));
}

#[test]
fn test_writer_identity_promotion_replaces_the_active_writer() {
    let (_dir, store) = store();
    store.claim_writer_identity("writer-a").unwrap();

    store.promote_writer_identity("writer-a", "writer-b").unwrap();

    assert_eq!(store.writer_identity().unwrap().as_deref(), Some("writer-b"));
    assert!(matches!(
        store.claim_writer_identity("writer-a").unwrap_err(),
        WriterIdentityError::Claimed { active, requested }
            if active == "writer-b" && requested == "writer-a"
    ));
}

#[test]
fn test_writer_identity_rejects_empty_values() {
    let (_dir, store) = store();

    assert!(matches!(
        store.claim_writer_identity("").unwrap_err(),
        WriterIdentityError::Empty
    ));
    store.claim_writer_identity("writer-a").unwrap();
    assert!(matches!(
        store.promote_writer_identity("writer-a", "").unwrap_err(),
        WriterIdentityError::Empty
    ));
}

#[test]
fn test_writer_identity_reads_a_store_without_the_table() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("older.redb");
    let database = redb::Database::create(&path).unwrap();
    let txn = database.begin_write().unwrap();
    txn.open_table(TableDefinition::<&str, u64>::new("serial")).unwrap();
    txn.commit().unwrap();
    drop(database);
    let store = MetaStore::open_existing(path).unwrap();

    assert_eq!(store.writer_identity().unwrap(), None);
    let error = store.promote_writer_identity("writer-a", "writer-b").unwrap_err();
    assert!(matches!(
        error,
        WriterIdentityError::Changed { active: None, expected } if expected == "writer-a"
    ));
    store.claim_writer_identity("writer-a").unwrap();
    assert_eq!(store.writer_identity().unwrap().as_deref(), Some("writer-a"));
}

#[test]
fn test_writer_identity_rejects_an_incompatible_table() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("incompatible.redb");
    let database = redb::Database::create(&path).unwrap();
    let txn = database.begin_write().unwrap();
    txn.open_table(TableDefinition::<&str, u64>::new("writer")).unwrap();
    txn.commit().unwrap();
    drop(database);
    let store = MetaStore::open_existing(path).unwrap();

    assert!(matches!(store.writer_identity(), Err(MetaError::Table(_))));
    assert!(matches!(
        store.claim_writer_identity("writer-a"),
        Err(WriterIdentityError::Store(MetaError::Table(_)))
    ));
}
