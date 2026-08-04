use crate::meta::{MetaStore, UploadRecord};

fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, store)
}

#[test]
fn test_begin_opens_a_session_at_offset_zero() {
    let (_dir, store) = store();

    store.begin_upload("session-1", "hosted", "app/image", 5).unwrap();

    assert_eq!(
        store.upload_record("session-1").unwrap(),
        Some(UploadRecord {
            offset: 0,
            index: "hosted".to_owned(),
            name: "app/image".to_owned(),
            updated_at_unix: 5,
        })
    );
}

#[test]
fn test_advance_records_the_new_offset() {
    let (_dir, store) = store();
    store.begin_upload("session-1", "hosted", "app/image", 5).unwrap();

    assert!(store.advance_upload("session-1", 4096, 9).unwrap());

    let record = store.upload_record("session-1").unwrap().unwrap();
    assert_eq!((record.offset, record.updated_at_unix), (4096, 9));
}

#[test]
fn test_advancing_an_unknown_session_reports_no_change() {
    let (_dir, store) = store();

    assert!(!store.advance_upload("ghost", 10, 1).unwrap());
    assert_eq!(store.upload_record("ghost").unwrap(), None);
}

#[test]
fn test_remove_closes_the_session_once() {
    let (_dir, store) = store();
    store.begin_upload("session-1", "hosted", "app/image", 5).unwrap();

    assert!(store.remove_upload("session-1").unwrap());
    assert!(!store.remove_upload("session-1").unwrap());
    assert_eq!(store.upload_record("session-1").unwrap(), None);
}

#[test]
fn test_reclaim_removes_idle_sessions_and_keeps_fresh_ones() {
    let (_dir, store) = store();
    store.begin_upload("idle", "hosted", "app/image", 50).unwrap();
    store.begin_upload("fresh", "hosted", "app/other", 100).unwrap();

    // A session untouched at or before the cutoff is reclaimed, boundary included; a fresh one survives.
    let reclaimed = store.reclaim_uploads(50, 10).unwrap();

    assert_eq!(reclaimed, vec!["idle".to_owned()]);
    assert_eq!(store.upload_record("idle").unwrap(), None);
    assert!(store.upload_record("fresh").unwrap().is_some());
}

#[test]
fn test_reclaim_honors_the_limit() {
    let (_dir, store) = store();
    store.begin_upload("a", "hosted", "app/a", 1).unwrap();
    store.begin_upload("b", "hosted", "app/b", 1).unwrap();

    let reclaimed = store.reclaim_uploads(50, 1).unwrap();

    assert_eq!(reclaimed.len(), 1);
    let remaining = ["a", "b"]
        .iter()
        .filter(|s| store.upload_record(s).unwrap().is_some())
        .count();
    assert_eq!(remaining, 1);
}
