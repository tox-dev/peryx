use crate::meta::MetaStore;

use super::store;

#[test]
fn test_copy_cursor_is_absent_before_the_first_pass() {
    let (_dir, store) = store();

    assert_eq!(store.blob_copy_cursor("west").unwrap(), None);
}

#[test]
fn test_copy_cursor_round_trips_where_the_scan_stopped() {
    let (_dir, store) = store();

    store.set_blob_copy_cursor("west", Some("sha256:0f1e")).unwrap();

    assert_eq!(store.blob_copy_cursor("west").unwrap(), Some("sha256:0f1e".to_owned()));
}

#[test]
fn test_copy_cursor_keeps_each_datacenter_apart() {
    let (_dir, store) = store();
    store.set_blob_copy_cursor("west", Some("sha256:0f1e")).unwrap();

    store.set_blob_copy_cursor("east", Some("sha256:aa01")).unwrap();

    assert_eq!(store.blob_copy_cursor("west").unwrap(), Some("sha256:0f1e".to_owned()));
}

#[test]
fn test_a_later_pass_overwrites_the_recorded_cursor() {
    let (_dir, store) = store();
    store.set_blob_copy_cursor("west", Some("sha256:0f1e")).unwrap();

    store.set_blob_copy_cursor("west", Some("sha256:aa01")).unwrap();

    assert_eq!(store.blob_copy_cursor("west").unwrap(), Some("sha256:aa01".to_owned()));
}

#[test]
fn test_clearing_the_cursor_restarts_the_next_pass() {
    let (_dir, store) = store();
    store.set_blob_copy_cursor("west", Some("sha256:0f1e")).unwrap();

    store.set_blob_copy_cursor("west", None).unwrap();

    assert_eq!(store.blob_copy_cursor("west").unwrap(), None);
}

#[test]
fn test_a_recorded_cursor_survives_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    MetaStore::open(&path)
        .unwrap()
        .set_blob_copy_cursor("west", Some("sha256:0f1e"))
        .unwrap();

    let reopened = MetaStore::open(&path).unwrap();

    assert_eq!(
        reopened.blob_copy_cursor("west").unwrap(),
        Some("sha256:0f1e".to_owned())
    );
}
