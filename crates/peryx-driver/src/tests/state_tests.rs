//! The transformed-page cache honors the byte budget it is configured with.

use bytes::Bytes;
use peryx_core::Ecosystem;
use peryx_identity::IndexAcl;
use peryx_index::{Index, IndexKind};
use peryx_policy::Policy;
use rstest::rstest;

use crate::rate_limit::RateLimitConfig;
use peryx_search::SearchParams;

use crate::state::{AppState, DEFAULT_HOT_CACHE_BYTES, ReadableFrontier, RuntimeOptions, SEARCH_VIEW};
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
            usage_retention_days: None,
            required_views: std::sync::Arc::from(crate::state::REQUIRED_VIEWS),
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
        ecosystem: Ecosystem::new("example"),
        kind,
        policy: Policy::default(),
        acl: IndexAcl::default(),
    }
}

fn replica_state() -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let meta = peryx_storage::meta::MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = peryx_storage::blob::BlobStore::new(dir.path().join("blobs"));
    let state = AppState::new(meta, blobs, 60, Vec::new());
    (dir, state)
}

/// Raise the store's authoritative serial by `count` journaled writes.
fn advance_authority(state: &AppState, count: usize) {
    state
        .meta
        .commit_driver_txn::<(), peryx_storage::meta::MetaError>(|txn| {
            for entry in 0..count {
                txn.put(&format!("k{entry}"), b"v")?;
            }
            Ok(((), (0..count).map(|entry| format!("j{entry}").into_bytes()).collect()))
        })
        .unwrap();
}

#[test]
fn test_fresh_replica_exposes_nothing_with_no_view_lagging() {
    let (_dir, state) = replica_state();

    assert_eq!(
        state.readable_frontier().unwrap(),
        ReadableFrontier {
            serial: 0,
            blocking: None,
        }
    );
}

#[test]
fn test_a_lagging_search_view_holds_readability_below_the_authority() {
    let (_dir, state) = replica_state();
    advance_authority(&state, 2);

    assert_eq!(
        state.readable_frontier().unwrap(),
        ReadableFrontier {
            serial: 0,
            blocking: Some(SEARCH_VIEW.to_owned()),
        }
    );
}

#[test]
fn test_a_caught_up_search_view_exposes_the_whole_authority_frontier() {
    let (_dir, state) = replica_state();
    advance_authority(&state, 2);
    state.meta.set_view_frontier(SEARCH_VIEW, 2).unwrap();

    assert_eq!(
        state.readable_frontier().unwrap(),
        ReadableFrontier {
            serial: 2,
            blocking: None,
        }
    );
}

#[test]
fn test_running_a_search_persists_the_view_frontier_and_lifts_readability() {
    let (_dir, state) = replica_state();
    advance_authority(&state, 2);
    assert_eq!(
        state.readable_frontier().unwrap(),
        ReadableFrontier {
            serial: 0,
            blocking: Some(SEARCH_VIEW.to_owned()),
        }
    );

    state
        .search
        .search(&state.search_ctx(), SearchParams::default())
        .unwrap();

    assert_eq!(
        state.readable_frontier().unwrap(),
        ReadableFrontier {
            serial: 2,
            blocking: None,
        }
    );
}

#[test]
fn test_write_ack_defaults_to_local_and_takes_the_installed_quorum() {
    let dir = tempfile::tempdir().unwrap();
    let meta = peryx_storage::meta::MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = peryx_storage::blob::BlobStore::new(dir.path().join("blobs"));
    let mut state = AppState::new(meta, blobs, 60, Vec::new());
    assert_eq!(state.write_ack_policy(), peryx_ha::DurabilityPolicy::Local);
    assert_eq!(state.write_ack_deadline(), std::time::Duration::from_secs(5));

    state.set_write_ack(peryx_ha::DurabilityPolicy::Majority, std::time::Duration::from_secs(30));
    assert_eq!(state.write_ack_policy(), peryx_ha::DurabilityPolicy::Majority);
    assert_eq!(state.write_ack_deadline(), std::time::Duration::from_secs(30));
}
