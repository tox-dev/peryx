use crate::meta::{CachedIndex, MetaStore};

fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("velox.redb")).unwrap();
    (dir, store)
}

fn record() -> CachedIndex {
    CachedIndex {
        etag: Some("\"abc\"".to_owned()),
        last_serial: Some(42),
        fetched_at_unix: 1_700_000_000,
        body: b"<html></html>".to_vec(),
    }
}

#[test]
fn test_serial_starts_at_zero_and_increments() {
    let (_dir, store) = store();
    assert_eq!(store.current_serial().unwrap(), 0);
    assert_eq!(store.next_serial().unwrap(), 1);
    assert_eq!(store.next_serial().unwrap(), 2);
    assert_eq!(store.current_serial().unwrap(), 2);
}

#[test]
fn test_put_and_get_index_roundtrip() {
    let (_dir, store) = store();
    assert_eq!(store.get_index("root/pypi/flask").unwrap(), None);
    store.put_index("root/pypi/flask", &record()).unwrap();
    assert_eq!(store.get_index("root/pypi/flask").unwrap(), Some(record()));
}

#[test]
fn test_put_index_overwrites() {
    let (_dir, store) = store();
    store.put_index("k", &record()).unwrap();
    let mut updated = record();
    updated.last_serial = Some(99);
    store.put_index("k", &updated).unwrap();
    assert_eq!(store.get_index("k").unwrap().unwrap().last_serial, Some(99));
}

#[test]
fn test_reopen_persists() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("velox.redb");
    {
        let store = MetaStore::open(&path).unwrap();
        store.next_serial().unwrap();
        store.put_index("k", &record()).unwrap();
    }
    let reopened = MetaStore::open(&path).unwrap();
    assert_eq!(reopened.current_serial().unwrap(), 1);
    assert_eq!(reopened.get_index("k").unwrap(), Some(record()));
}

#[test]
fn test_put_and_get_file_url() {
    let (_dir, store) = store();
    assert_eq!(store.get_file_url("deadbeef").unwrap(), None);
    store.put_file_url("deadbeef", "https://files.example/pkg.whl").unwrap();
    assert_eq!(
        store.get_file_url("deadbeef").unwrap().as_deref(),
        Some("https://files.example/pkg.whl")
    );
}

#[test]
fn test_put_and_get_metadata() {
    let (_dir, store) = store();
    assert_eq!(store.get_metadata("wheelsha").unwrap(), None);
    store
        .put_metadata("wheelsha", "https://up/pkg.whl.metadata", "metasha")
        .unwrap();
    assert_eq!(
        store.get_metadata("wheelsha").unwrap(),
        Some(("https://up/pkg.whl.metadata".to_owned(), "metasha".to_owned()))
    );
}

#[test]
fn test_put_and_list_projects() {
    let (_dir, store) = store();
    assert!(store.list_projects("root/pypi").unwrap().is_empty());
    store.put_project("root/pypi", "flask", "Flask").unwrap();
    store.put_project("root/pypi", "django", "Django").unwrap();
    store.put_project("other/index", "x", "X").unwrap();
    store.put_project("root/pypi", "flask", "Flask").unwrap(); // re-observe, no duplicate
    assert_eq!(store.list_projects("root/pypi").unwrap(), vec!["Django", "Flask"]);
}

#[test]
fn test_put_and_list_uploads() {
    let (_dir, store) = store();
    assert!(store.list_uploads("root/local", "flask").unwrap().is_empty());
    store
        .put_upload("root/local", "flask", "flask-2.0.whl", b"two")
        .unwrap();
    store
        .put_upload("root/local", "flask", "flask-1.0.whl", b"one")
        .unwrap();
    store
        .put_upload("root/local", "django", "django-4.0.whl", b"other")
        .unwrap();
    // Sorted by key (filename), and scoped to the one project.
    assert_eq!(
        store.list_uploads("root/local", "flask").unwrap(),
        vec![b"one".to_vec(), b"two".to_vec()]
    );
    assert_eq!(
        store.list_uploads("root/local", "django").unwrap(),
        vec![b"other".to_vec()]
    );
}

#[test]
fn test_put_upload_overwrites_same_filename() {
    let (_dir, store) = store();
    store
        .put_upload("root/local", "flask", "flask-1.0.whl", b"first")
        .unwrap();
    store
        .put_upload("root/local", "flask", "flask-1.0.whl", b"second")
        .unwrap();
    assert_eq!(
        store.list_uploads("root/local", "flask").unwrap(),
        vec![b"second".to_vec()]
    );
}

#[test]
fn test_cached_index_encode_decode_roundtrip() {
    assert_eq!(CachedIndex::decode(&record().encode()).unwrap(), record());
}

#[test]
fn test_cached_index_decode_rejects_garbage() {
    assert!(CachedIndex::decode(b"not json").is_err());
}
