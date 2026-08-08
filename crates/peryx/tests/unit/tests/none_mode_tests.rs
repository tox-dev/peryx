//! The `none` availability mode builds no availability resource: a single-node process holds no
//! availability record, route, metric, client, background task, timer, queue, or thread, while its
//! ordinary request surface stays intact. These probes hold that contract against the availability
//! subsystems on `main`: the replica loop, the `/+replication` routes, and the availability metric
//! families. The worker runtime (#502) and private control listener (#499) extend the same guarantee,
//! so their zero-cost checks join here once they merge.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use rstest::rstest;
use tower::ServiceExt as _;

use crate::config::{self, AvailabilityConfig, Config};
use crate::replication::ReplicationRuntime;
use crate::server::{build_router, build_state};

/// A zero-config single-node process: the omitted `[availability]` table resolves to `none`.
fn default_none(dir: &tempfile::TempDir) -> Config {
    Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default()
    }
}

/// A process that names `mode = "none"` explicitly, resolved through the real overlay pipeline, so the
/// probes cover the value an operator writes as well as the zero-config default.
fn explicit_none(dir: &tempfile::TempDir) -> Config {
    let overlay = config::from_toml("availability.toml".into(), "[availability]\nmode = \"none\"\n").unwrap();
    let mut config = Config::default().apply(overlay).unwrap();
    config.data_dir = dir.path().to_path_buf();
    config
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

#[rstest]
#[case::default(default_none)]
#[case::explicit(explicit_none)]
#[tokio::test]
async fn test_none_mode_spawns_no_availability_background_work(#[case] build: fn(&tempfile::TempDir) -> Config) {
    let dir = tempfile::tempdir().unwrap();
    let config = build(&dir);
    assert_eq!(config.availability, AvailabilityConfig::None);
    let state = build_state(&config).unwrap();
    assert!(!state.read_only, "none retains single-node write behavior");

    assert!(ReplicationRuntime::from_config(&config, &state).unwrap().is_none());
    let Err(error) = ReplicationRuntime::new(&config, &state) else {
        panic!("disabled availability must not construct a distributed runtime");
    };
    assert_eq!(
        error.to_string(),
        "distributed runtime requested while availability is disabled"
    );
}

#[rstest]
#[case::default(default_none)]
#[case::explicit(explicit_none)]
#[tokio::test]
async fn test_none_mode_mounts_no_availability_routes(#[case] build: fn(&tempfile::TempDir) -> Config) {
    let dir = tempfile::tempdir().unwrap();
    let config = build(&dir);
    let router = build_router(&config).unwrap();

    for path in [
        "/+replication/v1/health",
        "/+replication/v1/ready",
        "/+replication/v1/changes?after=0&limit=10",
    ] {
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
