//! The transformed-page cache honors the byte budget it is configured with.

use bytes::Bytes;
use peryx_core::Ecosystem;
use peryx_identity::IndexAcl;
use peryx_index::{Index, IndexKind};
use peryx_policy::Policy;
use rstest::rstest;

use crate::rate_limit::RateLimitConfig;
use crate::state::{AppState, DEFAULT_HOT_CACHE_BYTES, RuntimeOptions};
use peryx_events::webhook::WebhookRuntime;

#[test]
fn test_hot_cache_takes_the_configured_budget() {
    let (_dir, state) = state_with_budget(4096);
    assert_eq!(state.cache.hot.policy().max_capacity(), Some(4096));
}

#[test]
fn test_hot_cache_defaults_to_the_documented_budget() {
    let dir = tempfile::tempdir().unwrap();
    let meta = peryx_storage::meta::MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = peryx_storage::blob::BlobStore::new(dir.path().join("blobs"));
    let state = AppState::new(meta, blobs, 60, Vec::new());
    assert_eq!(state.cache.hot.policy().max_capacity(), Some(DEFAULT_HOT_CACHE_BYTES));
}

#[rstest]
#[case::exact("root/team", Some(("team", "")))]
#[case::root("root/other", Some(("root", "other")))]
#[case::nested("root/pypi/simple", Some(("pypi", "simple")))]
#[case::boundary("root/pypithon", Some(("root", "pypithon")))]
#[case::missing("elsewhere", None)]
fn test_repository_route_resolution(#[case] path: &str, #[case] expected: Option<(&'static str, &'static str)>) {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(
        peryx_storage::meta::MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        peryx_storage::blob::BlobStore::new(dir.path().join("blobs")),
        60,
        route_indexes(),
    );
    assert_eq!(
        state.resolve(path).map(|(index, rest)| (index.name.as_str(), rest)),
        expected
    );
}

#[test]
fn test_token_realm_is_unset_until_installed() {
    let dir = tempfile::tempdir().unwrap();
    let meta = peryx_storage::meta::MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = peryx_storage::blob::BlobStore::new(dir.path().join("blobs"));
    let mut state = AppState::new(meta, blobs, 60, Vec::new());
    assert!(state.signer.is_none());
    assert_eq!(state.token_ttl_secs, crate::state::DEFAULT_TOKEN_TTL_SECS);

    state.set_token_realm(peryx_identity::Signer::new(b"key", "peryx"), 900);
    assert!(state.signer.is_some());
    assert_eq!(state.token_ttl_secs, 900);
}

/// A zero budget turns the cache off: a warm page pays its transform again rather than being served
/// from memory. Asserted through the cache itself, so a knob that never reached moka would fail here.
#[test]
fn test_hot_cache_budget_of_zero_retains_nothing() {
    let (_dir, state) = state_with_budget(0);
    state.cache.hot.insert(
        "root/pypi\u{0}numpy".to_owned(),
        (Bytes::from_static(b"page"), i64::MAX, None),
    );
    state.cache.hot.run_pending_tasks();
    assert_eq!(state.cache.hot.get("root/pypi\u{0}numpy"), None);
}

fn state_with_budget(hot_cache_bytes: u64) -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let meta = peryx_storage::meta::MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = peryx_storage::blob::BlobStore::new(dir.path().join("blobs"));
    let state = AppState::with_search_path_and_runtime(
        meta,
        blobs,
        60,
        Vec::new(),
        dir.path().join("search-v1"),
        RuntimeOptions {
            rate_limit: RateLimitConfig::default(),
            upstream_concurrency: std::iter::empty(),
            upstream_routes: Vec::new(),
            webhooks: WebhookRuntime::disabled(),
            hot_cache_bytes,
            max_stale_secs: crate::DEFAULT_MAX_STALE_SECS,
        },
    )
    .unwrap();
    (dir, state)
}

fn route_indexes() -> Vec<Index> {
    vec![
        route_index("root", "root", IndexKind::Hosted { volatile: false }),
        route_index(
            "pypi",
            "root/pypi",
            IndexKind::Cached {
                client: peryx_upstream::UpstreamClient::new("https://pypi.org/simple/").unwrap(),
                offline: false,
            },
        ),
        route_index(
            "team",
            "root/team",
            IndexKind::Virtual {
                layers: vec![0, 1],
                upload: Some(0),
            },
        ),
    ]
}

fn route_index(name: &str, route: &str, kind: IndexKind) -> Index {
    Index {
        name: name.to_owned(),
        route: route.to_owned(),
        ecosystem: Ecosystem::Pypi,
        kind,
        policy: Policy::default(),
        acl: IndexAcl::default(),
    }
}
