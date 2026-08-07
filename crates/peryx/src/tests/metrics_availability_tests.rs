//! Metrics and analytics surfaces exercised across the `none`, `dc`, and `ha` availability modes.
//!
//! The `/metrics` exposition is unauthenticated, so its contract is which series a node exports, not who
//! may read them: general request and rate-limit series are present in every mode, the datacenter
//! durability series appear only where a datacenter durability decision is real (`dc`/`ha`), and the
//! replica sync and availability-worker series appear only on a read replica. The analytics surface is
//! the authenticated half: it stays operator-gated whatever the availability mode.
//!
//! Every arm is deterministic. Requests run through the in-process router with
//! [`ServiceExt::oneshot`], and a node's series are registered when its
//! [`ReplicationRuntime`](crate::replication::ReplicationRuntime) is built, so a scrape reads them
//! without a sync, a bound port, a sleep, or a real clock. The cardinality guard holds every exported
//! series to a bounded label vocabulary, so no operation id, digest, or repository can widen the scrape.

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
use crate::replication::ReplicationRuntime;
use crate::server::{build_state, router_for};

/// The shared secret a replica presents to its primary; irrelevant to the assertions but required to
/// build the runtime.
const TOKEN: &str = "replica-secret";
/// The password every seeded principal authenticates with.
const PASSWORD: &str = "metrics availability password";
/// The writer identity a `dc`/`ha` store is claimed under, so a replica's configured writer agrees with
/// the metadata store it follows.
const WRITER_IDENTITY: &str = "writer-a";
/// A dummy upstream a replica is configured to follow. It is never dialed: the runtime builds the client
/// but the tests read registered series rather than sync.
const UPSTREAM: &str = "https://primary.invalid/";

/// The general request series every node exports, whatever its mode.
const GENERAL_SERIES: &str = "peryx_requests_total";
/// The datacenter durability series a `dc`/`ha` node exports and a `none` node does not.
const DURABILITY_SERIES: &str = "peryx_dc_ack_durable_total";
/// The replica sync series only a read replica exports.
const REPLICATION_SERIES: &str = "peryx_ha_distributed_serial";
/// The availability worker series only a read replica exports.
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
                address: "10.0.0.1:8080".to_owned(),
                role: DcRole::Writer,
            },
            DcMember {
                node: "replica-b".to_owned(),
                dc: "east-2".to_owned(),
                address: "10.0.0.2:8080".to_owned(),
                role: DcRole::Replica,
            },
        ],
    }
}

/// Seed the replica store's writer identity before it is opened read-only, the way a provisioned replica
/// is handed the writer it will follow.
fn claim_writer(dir: &tempfile::TempDir) {
    MetaStore::open(dir.path().join("peryx.redb"))
        .unwrap()
        .claim_writer_identity(WRITER_IDENTITY)
        .unwrap();
}

fn node(config: &Config) -> (Arc<AppState>, Router, Option<ReplicationRuntime>) {
    let state = build_state(config).unwrap();
    let runtime = ReplicationRuntime::from_config(config, &state).unwrap();
    let router = runtime.as_ref().map_or_else(
        || router_for(state.clone()),
        |runtime| runtime.mount(router_for(state.clone())),
    );
    (state, router, runtime)
}

/// Create a principal holding `role` over the whole server and return its Basic `user:password`
/// credential.
async fn principal(state: &AppState, name: &str, role: Role) -> String {
    let id = state.users.create(name).unwrap().id;
    state.users.set_password(&id, PASSWORD).await.unwrap();
    state.authorization.grant(&id, role, GrantScope::Server).unwrap();
    format!("{name}:{PASSWORD}")
}

/// The `GET /metrics` exposition body as text.
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

/// The status of `GET /+analytics/top-packages` with an optional Basic credential.
async fn analytics_status(router: &Router, auth: Option<&str>) -> StatusCode {
    let mut request = Request::builder().uri("/+analytics/top-packages");
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
    let (_state, router, _runtime) = node(&none_config(&dir));

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
    let (_state, router, _runtime) = node(&build(&dir));

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
    let (state, router, _runtime) = node(&build(&dir));

    let body = scrape(&router).await;

    assert!(state.read_only, "a configured replica serves read-only");
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
    let (_state, router, _runtime) = node(&build(&dir));

    let body = scrape(&router).await;

    // A metric label drawn from an unbounded space would let one operation, artifact, or tenant multiply
    // the series a scrape returns; every exported series must key only on a closed vocabulary. The guard
    // matches the label-key syntax `name="`, so a help line's prose does not count as a label.
    for forbidden in ["operation", "digest", "repository", "project", "tenant", "traceparent"] {
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
    let (state, router, _runtime) = node(&build(&dir));
    let operator = principal(&state, "olivia", Role::Operator).await;
    let reader = principal(&state, "rita", Role::RepositoryReader).await;

    // Analytics carry cross-repository totals, so authorization is enforced at the boundary regardless of
    // the availability mode: only an analytics-bearing operator reads them. A repository reader without
    // the analytics scope is answered as if the endpoint did not exist, so a probe learns nothing.
    assert_eq!(analytics_status(&router, None).await, StatusCode::UNAUTHORIZED);
    assert_eq!(analytics_status(&router, Some(&reader)).await, StatusCode::NOT_FOUND);
    assert_eq!(analytics_status(&router, Some(&operator)).await, StatusCode::OK);
}
