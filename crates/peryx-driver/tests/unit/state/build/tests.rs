use std::sync::Arc;

use peryx_events::webhook::WebhookRuntime;
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;

use super::{AppState, DEFAULT_HOT_CACHE_BYTES, DEFAULT_MAX_STALE_SECS};
use crate::rate_limit::RateLimitConfig;

fn stores() -> (tempfile::TempDir, MetaStore, BlobStore) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    (dir, meta, blobs)
}

#[test]
fn test_rate_limit_constructor_keeps_default_runtime_controls() {
    let (_dir, meta, blobs) = stores();
    let state = AppState::with_rate_limits(meta, blobs, 60, Vec::new(), RateLimitConfig::default(), []);

    assert_eq!(state.serving.max_stale_secs, DEFAULT_MAX_STALE_SECS);
    assert_eq!(
        state.serving.cache.hot.policy().max_capacity(),
        Some(DEFAULT_HOT_CACHE_BYTES)
    );
}

#[test]
fn test_search_path_constructor_opens_persistent_search() {
    let (dir, meta, blobs) = stores();
    let state = AppState::with_search_path(meta, blobs, 60, Vec::new(), dir.path().join("search")).unwrap();

    assert_eq!(state.serving.ttl_secs, 60);
}

#[test]
fn test_search_path_rate_limit_constructor_accepts_overrides() {
    let (dir, meta, blobs) = stores();
    let state = AppState::with_search_path_and_rate_limits(
        meta,
        blobs,
        90,
        Vec::new(),
        dir.path().join("search"),
        RateLimitConfig::default(),
        [],
    )
    .unwrap();

    assert_eq!(state.serving.ttl_secs, 90);
}

#[test]
fn test_webhook_constructor_keeps_injected_clock() {
    let (_dir, meta, blobs) = stores();
    let state =
        AppState::with_clock_and_webhooks(meta, blobs, 60, Vec::new(), Arc::new(|| 41), WebhookRuntime::disabled());

    assert_eq!((state.serving.clock)(), 41);
}
