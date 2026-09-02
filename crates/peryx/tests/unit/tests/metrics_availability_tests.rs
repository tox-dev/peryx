//! Analytics remain operator-gated while metrics vary with the active availability services.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http_body_util::BodyExt as _;
use peryx_driver::state::AppState;
use peryx_identity::{GrantScope, Role};
use peryx_storage::meta::MetaStore;
use rstest::rstest;
use tower::ServiceExt as _;

use crate::config::{AvailabilityConfig, Config, DcMember, DcMembership, DcRole, ReplicationConfig, SecretSource};
use crate::server::{build_state, router_for};

const TOKEN: &str = "replica-secret";
const PASSWORD: &str = "metrics availability password";
const WRITER_IDENTITY: &str = "writer-a";
/// Avoids network I/O while constructing the replica runtime.
const UPSTREAM: &str = "https://primary.invalid/";

const GENERAL_SERIES: &str = "peryx_requests_total";
const DURABILITY_SERIES: &str = "peryx_dc_ack_durable_total";
const REPLICATION_SERIES: &str = "peryx_ha_distributed_serial";
const AVAILABILITY_SERIES: &str = "peryx_availability_worker_slots";

fn none_config(dir: &tempfile::TempDir) -> Config {
    Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default()
    }
}

fn dc_writer_config(dir: &tempfile::TempDir) -> Config {
    Config {
        data_dir: dir.path().to_path_buf(),
        writer_identity: Some(WRITER_IDENTITY.to_owned()),
        availability: AvailabilityConfig::Dc(primary_replication()),
        dc_membership: Some(group()),
        ..Config::default()
    }
}

fn dc_replica_config(dir: &tempfile::TempDir) -> Config {
    claim_writer(dir);
    Config {
        data_dir: dir.path().to_path_buf(),
        writer_identity: Some(WRITER_IDENTITY.to_owned()),
        availability: AvailabilityConfig::Dc(replica_replication()),
        ..Config::default()
    }
}

fn ha_replica_config(dir: &tempfile::TempDir) -> Config {
    claim_writer(dir);
    Config {
        data_dir: dir.path().to_path_buf(),
        writer_identity: Some(WRITER_IDENTITY.to_owned()),
        availability: AvailabilityConfig::Ha(replica_replication()),
        ..Config::default()
    }
}

fn primary_replication() -> ReplicationConfig {
    ReplicationConfig::Primary {
        source: WRITER_IDENTITY.to_owned(),
        token: SecretSource::Literal(TOKEN.to_owned()),
    }
}

fn replica_replication() -> ReplicationConfig {
    ReplicationConfig::Replica {
        upstream: UPSTREAM.to_owned(),
        token: SecretSource::Literal(TOKEN.to_owned()),
        poll_interval: Duration::from_millis(1),
        page_size: NonZeroUsize::MIN,
    }
}

fn group() -> DcMembership {
    DcMembership {
        group: "east".to_owned(),
        members: vec![
            DcMember {
                node: WRITER_IDENTITY.to_owned(),
                dc: "east-1".to_owned(),
                address: "http://10.0.0.1:8080".to_owned(),
                role: DcRole::Writer,
            },
            DcMember {
                node: "replica-b".to_owned(),
                dc: "east-2".to_owned(),
                address: "http://10.0.0.2:8080".to_owned(),
                role: DcRole::Replica,
            },
        ],
    }
}

/// Read-only opening requires prior writer identity provisioning.
fn claim_writer(dir: &tempfile::TempDir) {
    MetaStore::open(dir.path().join("peryx.redb"))
        .unwrap()
        .claim_writer_identity(WRITER_IDENTITY)
        .unwrap();
}

async fn node(config: &Config) -> (Arc<AppState>, Router, Option<peryx_ha_distributed::DistributedHandle>) {
    let state = build_state(config).unwrap();
    let plugins = crate::server::activate_plugins(config, &crate::compiled_plugins()).unwrap();
    let prepared = match &config.availability {
        AvailabilityConfig::None => None,
        AvailabilityConfig::Dc(_) | AvailabilityConfig::Ha(_) => Some(
            crate::process::prepare_distributed_availability(
                config,
                &plugins,
                &state,
                matches!(config.availability.mode(), peryx_ha::AvailabilityMode::Ha).then(|| {
                    crate::process::prepared_plain_availability_listener(
                        std::net::TcpListener::bind("127.0.0.1:0").unwrap(),
                    )
                    .unwrap()
                }),
            )
            .await
            .unwrap(),
        ),
    };
    let (availability, handle) = match prepared {
        Some(prepared) => (prepared.public_routes, Some(prepared.handle)),
        None => (Router::new(), None),
    };
    (state.clone(), router_for(state, availability), handle)
}

async fn principal(state: &AppState, name: &str, role: Role) -> String {
    let id = state.serving.users.create(name).unwrap().id;
    state.serving.users.set_password(&id, PASSWORD).await.unwrap();
    state
        .serving
        .authorization
        .grant(&id, role, GrantScope::Server)
        .unwrap();
    format!("{name}:{PASSWORD}")
}

async fn scrape(router: &Router) -> String {
    let response = router
        .clone()
        .oneshot(Request::builder().uri("/metrics").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "a scrape always answers");
    let bytes = response.into_body().collect().await.unwrap().to_bytes().to_vec();
    String::from_utf8(bytes).unwrap()
}

async fn analytics_status(router: &Router, auth: Option<&str>) -> StatusCode {
    let mut request = Request::builder().uri("/+analytics/top-resources");
    if let Some(auth) = auth {
        request = request.header(header::AUTHORIZATION, format!("Basic {}", STANDARD.encode(auth)));
    }
    router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn test_a_none_node_exports_general_series_and_no_availability_series() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, router, _runtime) = node(&none_config(&dir)).await;

    let body = scrape(&router).await;

    assert!(
        body.contains(GENERAL_SERIES),
        "general series are present in every mode: {body}"
    );
    for absent in [DURABILITY_SERIES, REPLICATION_SERIES, AVAILABILITY_SERIES] {
        assert!(
            !body.contains(absent),
            "a single-node none process runs no availability decision, so it exports no {absent}: {body}"
        );
    }
}

#[rstest]
#[case::none(none_config as fn(&tempfile::TempDir) -> Config, false)]
#[case::dc_writer(dc_writer_config as fn(&tempfile::TempDir) -> Config, true)]
#[tokio::test]
async fn test_the_durability_series_appears_only_in_an_enabled_mode(
    #[case] build: fn(&tempfile::TempDir) -> Config,
    #[case] enabled: bool,
) {
    let dir = tempfile::tempdir().unwrap();
    let (_state, router, _runtime) = node(&build(&dir)).await;

    let body = scrape(&router).await;

    assert!(body.contains(GENERAL_SERIES), "general series stay present: {body}");
    assert_eq!(
        body.contains(DURABILITY_SERIES),
        enabled,
        "the durability series appears exactly when a datacenter durability decision is real: {body}"
    );
}

#[rstest]
#[case::dc_replica(dc_replica_config as fn(&tempfile::TempDir) -> Config)]
#[case::ha_replica(ha_replica_config as fn(&tempfile::TempDir) -> Config)]
#[tokio::test]
async fn test_a_replica_exports_the_replication_and_availability_series(
    #[case] build: fn(&tempfile::TempDir) -> Config,
) {
    let dir = tempfile::tempdir().unwrap();
    let (state, router, _runtime) = node(&build(&dir)).await;

    let body = scrape(&router).await;

    assert!(state.serving.read_only, "a configured replica serves read-only");
    for series in [
        GENERAL_SERIES,
        REPLICATION_SERIES,
        AVAILABILITY_SERIES,
        DURABILITY_SERIES,
    ] {
        assert!(body.contains(series), "a replica exports {series}: {body}");
    }
}

#[rstest]
#[case::none(none_config as fn(&tempfile::TempDir) -> Config)]
#[case::dc_writer(dc_writer_config as fn(&tempfile::TempDir) -> Config)]
#[case::dc_replica(dc_replica_config as fn(&tempfile::TempDir) -> Config)]
#[case::ha_replica(ha_replica_config as fn(&tempfile::TempDir) -> Config)]
#[tokio::test]
async fn test_metrics_carry_no_high_cardinality_label(#[case] build: fn(&tempfile::TempDir) -> Config) {
    let dir = tempfile::tempdir().unwrap();
    let (_state, router, _runtime) = node(&build(&dir)).await;

    let body = scrape(&router).await;

    // Unbounded operation, artifact, or tenant labels would multiply exported series.
    for forbidden in ["operation", "digest", "repository", "resource", "tenant", "traceparent"] {
        assert!(
            !body.contains(&format!("{forbidden}=\"")),
            "an exported series carries a high-cardinality label {forbidden}: {body}"
        );
    }
}

#[rstest]
#[case::none(none_config as fn(&tempfile::TempDir) -> Config)]
#[case::dc_writer(dc_writer_config as fn(&tempfile::TempDir) -> Config)]
#[tokio::test]
async fn test_analytics_stays_operator_gated_whatever_the_mode(#[case] build: fn(&tempfile::TempDir) -> Config) {
    let dir = tempfile::tempdir().unwrap();
    let (state, router, _runtime) = node(&build(&dir)).await;
    let operator = principal(&state, "olivia", Role::Operator).await;
    let reader = principal(&state, "rita", Role::RepositoryReader).await;

    // Returning 404 prevents unauthorized callers from learning that aggregate analytics exist.
    assert_eq!(analytics_status(&router, None).await, StatusCode::UNAUTHORIZED);
    assert_eq!(analytics_status(&router, Some(&reader)).await, StatusCode::NOT_FOUND);
    assert_eq!(analytics_status(&router, Some(&operator)).await, StatusCode::OK);
}
