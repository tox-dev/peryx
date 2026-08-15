use std::collections::HashSet;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use redb::{ReadableDatabase as _, TableHandle as _};
use rstest::rstest;
use tower::ServiceExt as _;

use crate::config::{self, AvailabilityConfig, Config};
use crate::server::{build_router, build_state, build_state_with_active_plugins, router_for};

use super::support::plugins;

const DISTRIBUTED_TABLES: [&str; 18] = [
    "artifact_placement",
    "blob_placement",
    "blob_chunk_digest",
    "blob_reclaim_guard",
    "derived_view_frontier",
    "ingress_intent",
    "ingress_intent_count",
    "ingress_intent_order",
    "ingress_intent_seq",
    "journal",
    "journal_blobs",
    "journal_mutations",
    "reclamation_tombstone",
    "reconcile_backlog",
    "transfer_attempt",
    "transfer_audit",
    "visibility_snapshot",
    "writer",
];

const DISTRIBUTED_ROUTES: [&str; 21] = [
    "/+analytics/completeness",
    "/+availability/operations",
    "/+availability/placements",
    "/+availability/placements/sha256:abc",
    "/+availability/topology",
    "/+availability/topology/stream",
    "/+replication/v1/analytics",
    "/+replication/v1/blobs/sha256/abc",
    "/+replication/v1/health",
    "/+replication/v1/ready",
    "/+replication/v1/changes?after=0&limit=10",
    "/+replication/v1/frontier/example",
    "/+replication/v1/heartbeat",
    "/+replication/v1/raft/append-entries",
    "/+replication/v1/raft/install-snapshot",
    "/+replication/v1/raft/vote",
    "/+replication/v1/receipts/sha256/abc",
    "/availability/v1/commands",
    "/availability/v1/status",
    "/availability/v1/transfers",
    "/availability/v1/transfers/example",
];

fn default_none(dir: &tempfile::TempDir) -> Config {
    Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default()
    }
}

/// Covers the operator-facing value through the overlay pipeline.
fn explicit_none(dir: &tempfile::TempDir) -> Config {
    let overlay = config::from_toml("availability.toml".into(), "[availability]\nmode = \"none\"\n").unwrap();
    let mut config = Config::default().apply(overlay).unwrap();
    config.data_dir = dir.path().to_path_buf();
    config
}

fn read_only_none(dir: &tempfile::TempDir) -> Config {
    Config {
        data_dir: dir.path().to_path_buf(),
        read_only: true,
        ..Config::default()
    }
}

async fn status_of(router: &Router, path: &str) -> StatusCode {
    router
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap()
        .status()
}

async fn metrics_body(router: &Router) -> String {
    let response = router
        .clone()
        .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn table_names(path: &std::path::Path) -> HashSet<String> {
    let database = redb::Database::open(path).unwrap();
    let read = database.begin_read().unwrap();
    read.list_tables()
        .unwrap()
        .map(|table| table.name().to_owned())
        .collect()
}

#[rstest]
#[case::default(default_none)]
#[case::explicit(explicit_none)]
fn test_none_mode_selects_local_configuration(#[case] build: fn(&tempfile::TempDir) -> Config) {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(build(&dir).availability, AvailabilityConfig::None);
}

#[rstest]
#[case::default(default_none, false)]
#[case::explicit(explicit_none, false)]
#[case::read_only(read_only_none, true)]
fn test_none_mode_starts_without_distributed_tables(
    #[case] build: fn(&tempfile::TempDir) -> Config,
    #[case] expected_read_only: bool,
) {
    let dir = tempfile::tempdir().unwrap();
    let config = build(&dir);
    let state = build_state(&config).unwrap();
    assert_eq!(state.serving.read_only, expected_read_only);
    drop(state);

    let tables = table_names(&config.data_dir.join("peryx.redb"));
    for table in DISTRIBUTED_TABLES {
        assert!(!tables.contains(table), "{table}");
    }
}

#[rstest]
#[case::default(default_none)]
#[case::explicit(explicit_none)]
#[tokio::test]
async fn test_none_mode_spawns_no_availability_background_work(#[case] build: fn(&tempfile::TempDir) -> Config) {
    let dir = tempfile::tempdir().unwrap();
    let config = build(&dir);
    assert_eq!(config.availability, AvailabilityConfig::None);
    let state = build_state(&config).unwrap();
    assert!(!state.serving.read_only, "none retains single-node write behavior");

    assert_eq!(
        state.serving.availability_topology().mode,
        peryx_core::TopologyMode::None
    );
}

#[tokio::test]
async fn test_none_mode_process_constructs_no_distributed_resources() {
    let dir = tempfile::tempdir().unwrap();
    let config = default_none(&dir);
    let plugins = crate::compiled_plugins();
    let active = crate::server::activate_plugins(&config, &plugins).unwrap();
    let state = build_state_with_active_plugins(&config, &active).unwrap();

    let availability = crate::process::prepare_process_availability(&config, &active, &state)
        .await
        .unwrap();
    assert!(availability.is_none());
}

#[rstest]
#[case::default(default_none)]
#[case::explicit(explicit_none)]
#[tokio::test]
async fn test_none_mode_mounts_no_availability_routes(#[case] build: fn(&tempfile::TempDir) -> Config) {
    let dir = tempfile::tempdir().unwrap();
    let config = build(&dir);
    let router = build_router(&config).unwrap();

    for path in DISTRIBUTED_ROUTES {
        assert_eq!(status_of(&router, path).await, StatusCode::NOT_FOUND, "{path}");
    }
    assert_eq!(
        status_of(&router, "/metrics").await,
        StatusCode::OK,
        "ordinary routes stay mounted"
    );
}

#[rstest]
#[case::default(default_none)]
#[case::explicit(explicit_none)]
#[tokio::test]
async fn test_none_mode_registers_no_availability_metrics(#[case] build: fn(&tempfile::TempDir) -> Config) {
    let dir = tempfile::tempdir().unwrap();
    let config = build(&dir);
    let router = build_router(&config).unwrap();

    let body = metrics_body(&router).await;
    assert!(!body.contains("peryx_ha_distributed_"), "{body}");
    assert!(!body.contains("peryx_availability_"), "{body}");
    assert!(
        body.contains("peryx_requests_total"),
        "ordinary request metrics remain: {body}"
    );
}

#[rstest]
#[case::default(default_none)]
#[case::explicit(explicit_none)]
#[tokio::test]
async fn test_none_mode_hosted_write_allocates_no_distributed_resources(
    #[case] build: fn(&tempfile::TempDir) -> Config,
) {
    let dir = tempfile::tempdir().unwrap();
    let plugins = plugins();
    let mut config = Config::with_plugins(&plugins);
    config.data_dir = dir.path().to_path_buf();
    config.availability = build(&dir).availability;
    let plugins = crate::server::activate_plugins(&config, &plugins).unwrap();
    let state = build_state_with_active_plugins(&config, &plugins).unwrap();

    assert_eq!(
        state.serving.availability_topology().mode,
        peryx_core::TopologyMode::None
    );
    assert!(config.availability_listener.is_none());
    let router = router_for(state.clone());
    let response = router
        .clone()
        .oneshot(
            Request::post("/+fixture/upload")
                .body(Body::from("hosted artifact"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    for path in DISTRIBUTED_ROUTES {
        assert_eq!(status_of(&router, path).await, StatusCode::NOT_FOUND, "{path}");
    }
    let body = metrics_body(&router).await;
    assert!(!body.contains("peryx_ha_distributed_"), "{body}");
    assert!(!body.contains("peryx_availability_"), "{body}");
    drop(router);
    drop(state);

    let tables = table_names(&config.data_dir.join("peryx.redb"));
    for table in DISTRIBUTED_TABLES {
        assert!(!tables.contains(table), "{table}");
    }
}
