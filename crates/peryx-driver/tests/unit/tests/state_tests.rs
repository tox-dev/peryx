use std::sync::Arc;

use peryx_core::Ecosystem;
use peryx_events::webhook::WebhookRuntime;
use peryx_identity::IndexAcl;
use peryx_index::{Index, IndexKind};
use peryx_policy::Policy;
use peryx_search::SearchParams;
use peryx_storage::meta::MetaStore;
use peryx_upstream::{NamedUpstream, UpstreamClient, UpstreamRouter};
use rstest::rstest;

use crate::http_services::HttpDomainServices;
use crate::rate_limit::RateLimitConfig;
use crate::state::{AppState, DEFAULT_HOT_CACHE_BYTES, ReadableFrontier, RuntimeOptions, SEARCH_VIEW, ServingState};

#[test]
fn test_describe_indexes_includes_runtime_upstream_status() {
    let dir = tempfile::tempdir().unwrap();
    let meta = peryx_storage::meta::MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = peryx_storage::blob::BlobStore::new(dir.path().join("blobs"));
    let index = route_index(
        "alpha",
        "alpha",
        IndexKind::Cached {
            client: UpstreamClient::new("https://fallback.example/artifacts/").unwrap(),
            offline: false,
        },
    );
    let state = AppState::with_search_path_and_runtime(
        meta,
        blobs,
        60,
        vec![index],
        dir.path().join("search-v1"),
        RuntimeOptions {
            rate_limit: RateLimitConfig::default(),
            upstream_concurrency: std::iter::empty(),
            upstream_routes: vec![(
                "alpha".to_owned(),
                UpstreamRouter::new(vec![NamedUpstream::new(
                    "origin",
                    UpstreamClient::new("https://origin.example/artifacts/").unwrap(),
                )])
                .unwrap(),
            )],
            webhooks: WebhookRuntime::disabled(),
            hot_cache_bytes: DEFAULT_HOT_CACHE_BYTES,
            max_stale_secs: crate::DEFAULT_MAX_STALE_SECS,
            usage_retention_days: None,
            required_views: std::sync::Arc::from(crate::state::REQUIRED_VIEWS),
        },
    )
    .unwrap();

    assert_eq!(
        state.serving.describe_indexes()[0]
            .upstream
            .as_ref()
            .unwrap()
            .sources
            .len(),
        1
    );
}

#[rstest]
#[case::exact("root/team", Some(("team", "")))]
#[case::root("root/other", Some(("root", "other")))]
#[case::nested("root/alpha/items", Some(("alpha", "items")))]
#[case::boundary("root/alphathon", Some(("root", "alphathon")))]
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
        state
            .serving
            .resolve(path)
            .map(|(index, rest)| (index.name.as_str(), rest)),
        expected
    );
}

fn route_indexes() -> Vec<Index> {
    vec![
        route_index("root", "root", IndexKind::Hosted { volatile: false }),
        route_index(
            "alpha",
            "root/alpha",
            IndexKind::Cached {
                client: peryx_upstream::UpstreamClient::new("https://upstream.example/artifacts/").unwrap(),
                offline: false,
            },
        ),
        route_index(
            "team",
            "root/team",
            IndexKind::Virtual {
                layers: vec![0, 1],
                write_target: Some(0),
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

fn replica_state() -> (tempfile::TempDir, Arc<ServingState>, MetaStore, HttpDomainServices) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    meta.initialize_distributed_state().unwrap();
    let blobs = peryx_storage::blob::BlobStore::new(dir.path().join("blobs"));
    let state = Arc::new(AppState::new(meta.clone(), blobs, 60, Vec::new()));
    let services = HttpDomainServices::for_state(&state);
    (dir, Arc::clone(&state.serving), meta, services)
}

fn advance_authority(meta: &MetaStore, count: usize) {
    meta.commit_driver_txn::<(), peryx_storage::meta::MetaError>(|txn| {
        for entry in 0..count {
            txn.put(&format!("k{entry}"), b"v")?;
        }
        Ok(((), (0..count).map(|entry| format!("j{entry}").into_bytes()).collect()))
    })
    .unwrap();
}

#[test]
fn test_fresh_replica_exposes_nothing_with_no_view_lagging() {
    let (_dir, state, _meta, _services) = replica_state();

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
    let (_dir, state, meta, _services) = replica_state();
    advance_authority(&meta, 2);

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
    let (_dir, state, meta, _services) = replica_state();
    advance_authority(&meta, 2);
    meta.set_view_frontier(SEARCH_VIEW, 2).unwrap();

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
    let (_dir, state, meta, services) = replica_state();
    advance_authority(&meta, 2);
    assert_eq!(
        state.readable_frontier().unwrap(),
        ReadableFrontier {
            serial: 0,
            blocking: Some(SEARCH_VIEW.to_owned()),
        }
    );

    services.search().search(SearchParams::default(), None).unwrap();

    assert_eq!(
        state.readable_frontier().unwrap(),
        ReadableFrontier {
            serial: 2,
            blocking: None,
        }
    );
}
