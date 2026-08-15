use std::sync::Arc;

use bytes::Bytes;
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;

use crate::state::{AppState, ServingState};

fn state() -> (tempfile::TempDir, Arc<ServingState>) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let state = AppState::with_clock(meta, blobs, 60, Vec::new(), Arc::new(|| 100));
    (dir, state.serving)
}

#[test]
fn test_hot_accessors_honor_expiry_and_revision() {
    let (_dir, state) = state();
    let key = state.representation_key("resources", "resource", "page");
    state
        .cache
        .store_hot_versioned(key.clone(), Bytes::from_static(b"page"), 101, Some(7));

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
    state.remember_negative("expired".to_owned(), 0);
    assert!(!state.negative_fresh("expired"));
    assert!(!state.negative_fresh("unknown"));
}

#[test]
fn test_resource_invalidation_advances_only_its_representation_key() {
    let (_dir, state) = state();
    let resource = state.representation_key("resources", "resource", "page");
    let other = state.representation_key("resources", "other", "page");

    state.invalidate_resource("resource");

    assert_ne!(state.representation_key("resources", "resource", "page"), resource);
    assert_eq!(state.representation_key("resources", "other", "page"), other);
}

#[test]
fn test_representation_only_invalidation_advances_representation_key() {
    let (_dir, state) = state();
    let key = state.representation_key("resources", "resource", "page");

    state.invalidate_representations("resource");

    assert_ne!(state.representation_key("resources", "resource", "page"), key);
}

#[test]
fn test_search_epoch_can_advance_without_hot_invalidation() {
    let (_dir, state) = state();
    let key = state.representation_key("resources", "resource", "page");

    state.bump_search_epoch();

    assert_eq!(state.representation_key("resources", "resource", "page"), key);
}
