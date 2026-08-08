use std::sync::Arc;

use bytes::Bytes;
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;

use crate::state::AppState;

fn state() -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    (dir, AppState::with_clock(meta, blobs, 60, Vec::new(), Arc::new(|| 100)))
}

#[test]
fn test_hot_accessors_honor_expiry_and_revision() {
    let (_dir, state) = state();
    let key = state.hot_key("packages", "project", "page");
    state
        .cache
        .hot
        .insert(key.clone(), (Bytes::from_static(b"page"), 101, Some(7)));

    assert_eq!(state.hot_fresh(&key), Some(Bytes::from_static(b"page")));
    assert_eq!(
        state.hot_fresh_versioned(&key),
        Some((Bytes::from_static(b"page"), Some(7)))
    );
}

#[test]
fn test_negative_accessors_expire_against_injected_clock() {
    let (_dir, state) = state();
    state.remember_negative("missing".to_owned(), 1);

    assert!(state.negative_fresh("missing"));
    state.cache.negative.insert("expired".to_owned(), 100);
    assert!(!state.negative_fresh("expired"));
    assert!(!state.negative_fresh("unknown"));
}

#[test]
fn test_project_invalidation_advances_only_its_hot_key() {
    let (_dir, state) = state();
    let project = state.hot_key("packages", "project", "page");
    let other = state.hot_key("packages", "other", "page");

    state.invalidate_project("project");

    assert_ne!(state.hot_key("packages", "project", "page"), project);
    assert_eq!(state.hot_key("packages", "other", "page"), other);
}

#[test]
fn test_hot_only_invalidation_advances_project_key() {
    let (_dir, state) = state();
    let key = state.hot_key("packages", "project", "page");

    state.invalidate_hot_pages("project");

    assert_ne!(state.hot_key("packages", "project", "page"), key);
}

#[test]
fn test_search_epoch_can_advance_without_hot_invalidation() {
    let (_dir, state) = state();
    let key = state.hot_key("packages", "project", "page");

    state.bump_search_epoch();

    assert_eq!(state.hot_key("packages", "project", "page"), key);
}
