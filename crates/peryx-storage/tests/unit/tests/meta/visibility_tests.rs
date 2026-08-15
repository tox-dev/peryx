use peryx_ha::VisibilitySnapshotStore;
use tempfile::TempDir;

use crate::meta::MetaStore;

fn store() -> (TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, store)
}

#[test]
fn test_visibility_snapshot_is_absent_before_the_first_save() {
    let (_dir, store) = store();
    assert_eq!(store.visibility_snapshot().unwrap(), None);
}

#[test]
fn test_visibility_snapshot_round_trips_the_saved_bytes() {
    let (_dir, store) = store();
    let snapshot = b"opaque-visibility-apply-state".to_vec();

    store.save_visibility_snapshot(&snapshot).unwrap();

    assert_eq!(store.visibility_snapshot().unwrap(), Some(snapshot));
}

#[test]
fn test_saving_a_snapshot_overwrites_the_prior_one() {
    let (_dir, store) = store();
    store.save_visibility_snapshot(b"first").unwrap();

    store.save_visibility_snapshot(b"second").unwrap();

    assert_eq!(store.visibility_snapshot().unwrap(), Some(b"second".to_vec()));
}

#[test]
fn test_a_saved_snapshot_survives_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    MetaStore::open(&path)
        .unwrap()
        .save_visibility_snapshot(b"durable")
        .unwrap();

    let reopened = MetaStore::open(&path).unwrap();

    assert_eq!(reopened.visibility_snapshot().unwrap(), Some(b"durable".to_vec()));
}

#[test]
fn test_visibility_trait_round_trips_a_snapshot() {
    let (_dir, store) = store();

    <MetaStore as VisibilitySnapshotStore>::save_snapshot(&store, b"trait-state").unwrap();

    assert_eq!(
        <MetaStore as VisibilitySnapshotStore>::load_snapshot(&store).unwrap(),
        Some(b"trait-state".to_vec())
    );
}
