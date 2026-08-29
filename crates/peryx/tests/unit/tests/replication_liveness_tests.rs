use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http_body_util::BodyExt as _;
use peryx_driver::state::AppState;
use peryx_ha::DurabilityPolicy;
use peryx_identity::{GrantScope, Role};
use rstest::rstest;
use serde_json::{Value, json};
use tower::ServiceExt as _;

use crate::config::{AvailabilityConfig, Config, DcMember, DcMembership, DcRole, ReplicationConfig, SecretSource};
use crate::replication::ReplicationRuntime;
use crate::server::{build_state, router_for};

const TOKEN: &str = "group-secret";
const PASSWORD: &str = "operator password";

fn member(node: &str, role: DcRole) -> DcMember {
    DcMember {
        node: node.to_owned(),
        dc: format!("dc-{node}"),
        address: format!("https://{node}.example/"),
        role,
    }
}

fn primary_with_roster(dir: &tempfile::TempDir) -> Config {
    Config {
        data_dir: dir.path().to_path_buf(),
        availability: AvailabilityConfig::Dc(ReplicationConfig::Primary {
            source: "writer-a".to_owned(),
            token: SecretSource::Literal(TOKEN.to_owned()),
        }),
        dc_membership: Some(DcMembership {
            group: "group-a".to_owned(),
            members: vec![member("writer-a", DcRole::Writer), member("replica-a", DcRole::Replica)],
        }),
        ..Config::default()
    }
}

async fn operator(state: &AppState) -> String {
    let user = state.serving.users.create("Olivia").unwrap();
    state.serving.users.set_password(&user.id, PASSWORD).await.unwrap();
    state
        .serving
        .authorization
        .grant(&user.id, Role::Operator, GrantScope::Server)
        .unwrap();
    format!("Olivia:{PASSWORD}")
}

async fn administrator(state: &AppState) -> String {
    let user = state.serving.users.create("Ada").unwrap();
    state.serving.users.set_password(&user.id, PASSWORD).await.unwrap();
    state
        .serving
        .authorization
        .grant(&user.id, Role::Administrator, GrantScope::Server)
        .unwrap();
    format!("Ada:{PASSWORD}")
}

async fn prepare(
    config: &Config,
    state: &std::sync::Arc<AppState>,
) -> peryx_ha::PreparedAvailability<Router, peryx_ha_distributed::DistributedHandle> {
    ReplicationRuntime::new(config, state)
        .unwrap()
        .prepare(
            state,
            peryx_ha_distributed::reference_inventory(
                crate::compiled_plugins().drivers().clone(),
                state.serving.meta.clone(),
                config.indexes.iter().map(|index| index.name.clone()).collect(),
            ),
            Some(
                crate::process::prepared_plain_availability_listener(
                    std::net::TcpListener::bind("127.0.0.1:0").unwrap(),
                )
                .unwrap(),
            ),
        )
        .await
        .unwrap()
}

async fn heartbeat(router: &Router, report: Value) -> StatusCode {
    router
        .clone()
        .oneshot(
            Request::post("/+replication/v1/heartbeat")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Body::from(serde_json::to_vec(&report).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

async fn health(router: &Router, credentials: Option<&str>) -> Value {
    let mut request = Request::get("/+replication/v1/health");
    if let Some(credentials) = credentials {
        request = request.header(header::AUTHORIZATION, format!("Basic {}", STANDARD.encode(credentials)));
    }
    let body = router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test]
async fn test_primary_surfaces_replica_liveness_to_an_operator() {
    let dir = tempfile::tempdir().unwrap();
    let config = primary_with_roster(&dir);
    let state = build_state(&config).unwrap();
    let operator = operator(&state).await;
    let runtime = ReplicationRuntime::new(&config, &state).unwrap();
    let router = runtime.mount(router_for(state));

    let accepted = heartbeat(&router, json!({"node": "replica-a", "incarnation": 1, "sequence": 1})).await;
    assert_eq!(accepted, StatusCode::ACCEPTED);

    let document = health(&router, Some(&operator)).await;
    let peers = document["peers"].as_array().unwrap();
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0]["node"], "replica-a");
    assert_eq!(peers[0]["suspicion"], "alive");
    assert_eq!(peers[0]["sequence"], 1);
}

#[tokio::test]
async fn test_liveness_hint_stays_hidden_from_an_unauthenticated_caller() {
    let dir = tempfile::tempdir().unwrap();
    let config = primary_with_roster(&dir);
    let state = build_state(&config).unwrap();
    let runtime = ReplicationRuntime::new(&config, &state).unwrap();
    let router = runtime.mount(router_for(state));

    let document = health(&router, None).await;

    assert!(document.get("peers").is_none());
    assert_eq!(document["role"], "primary");
}

#[tokio::test]
async fn test_primary_without_a_roster_mounts_no_heartbeat_ingest() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = primary_with_roster(&dir);
    config.dc_membership = None;
    let state = build_state(&config).unwrap();
    let operator = operator(&state).await;
    let runtime = ReplicationRuntime::new(&config, &state).unwrap();
    let router = runtime.mount(router_for(state));

    let status = heartbeat(&router, json!({"node": "replica-a", "incarnation": 1, "sequence": 1})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let document = health(&router, Some(&operator)).await;
    assert!(document.get("peers").is_none());
}

#[tokio::test]
async fn test_group_readiness_blocks_until_the_replica_reports_then_clears() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = primary_with_roster(&dir);
    config.write_ack.policy = DurabilityPolicy::Majority;
    let state = build_state(&config).unwrap();
    let operator = operator(&state).await;
    let runtime = ReplicationRuntime::new(&config, &state).unwrap();
    let router = runtime.mount(router_for(state));

    let readiness = health(&router, Some(&operator)).await["group_readiness"].clone();
    assert_eq!(readiness["ready"], json!(false));
    assert_eq!(readiness["policy"], "majority");
    assert_eq!(readiness["durable_frontier"], json!(0));
    assert_eq!(readiness["blocked"]["insufficient_members"]["reporting"], json!(1));
    assert_eq!(readiness["blocked"]["insufficient_members"]["required"], json!(2));

    assert_eq!(
        heartbeat(
            &router,
            json!({"node": "replica-a", "incarnation": 1, "sequence": 1, "applied": 0})
        )
        .await,
        StatusCode::ACCEPTED,
    );
    let readiness = health(&router, Some(&operator)).await["group_readiness"].clone();
    assert_eq!(readiness["ready"], json!(true));
    assert_eq!(readiness["blocked"], Value::Null);
    assert_eq!(readiness["durable_frontier"], json!(0));
}

#[rstest]
#[case::local(
    DurabilityPolicy::Local,
    json!({"ready": true, "durable_frontier": 0, "policy": "local", "blocked": null})
)]
#[case::majority(
    DurabilityPolicy::Majority,
    json!({
        "ready": false,
        "durable_frontier": 0,
        "policy": "majority",
        "blocked": {"insufficient_members": {"reporting": 1, "required": 2}},
    })
)]
#[case::everywhere(
    DurabilityPolicy::Everywhere,
    json!({
        "ready": false,
        "durable_frontier": 0,
        "policy": "everywhere",
        "blocked": {"insufficient_members": {"reporting": 1, "required": 3}},
    })
)]
#[tokio::test]
async fn test_group_readiness_uses_the_write_ack_policy(#[case] policy: DurabilityPolicy, #[case] expected: Value) {
    let dir = tempfile::tempdir().unwrap();
    let mut config = primary_with_roster(&dir);
    config.write_ack.policy = policy;
    config
        .dc_membership
        .as_mut()
        .unwrap()
        .members
        .push(member("replica-b", DcRole::Replica));
    let state = build_state(&config).unwrap();
    let operator = operator(&state).await;
    let runtime = ReplicationRuntime::new(&config, &state).unwrap();
    let readiness = health(&runtime.mount(router_for(state)), Some(&operator)).await["group_readiness"].clone();

    assert_eq!(readiness, expected);
}

#[tokio::test]
async fn test_group_readiness_stays_hidden_from_an_unauthenticated_caller() {
    let dir = tempfile::tempdir().unwrap();
    let config = primary_with_roster(&dir);
    let state = build_state(&config).unwrap();
    let runtime = ReplicationRuntime::new(&config, &state).unwrap();
    let router = runtime.mount(router_for(state));

    assert!(health(&router, None).await.get("group_readiness").is_none());
}

#[tokio::test]
async fn test_a_rosterless_primary_publishes_no_group_readiness() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = primary_with_roster(&dir);
    config.dc_membership = None;
    let state = build_state(&config).unwrap();
    let operator = operator(&state).await;
    let runtime = ReplicationRuntime::new(&config, &state).unwrap();
    let router = runtime.mount(router_for(state));

    assert!(health(&router, Some(&operator)).await.get("group_readiness").is_none());
}

#[tokio::test]
async fn test_prepared_runtime_serves_authenticated_control_routes() {
    let dir = tempfile::tempdir().unwrap();
    let config = primary_with_roster(&dir);
    let state = build_state(&config).unwrap();
    let credentials = administrator(&state).await;
    let private = prepare(&config, &state).await.private_routes.unwrap();
    let authorization = format!("Basic {}", STANDARD.encode(credentials));

    let status = private
        .clone()
        .oneshot(
            Request::get("/availability/v1/status")
                .header(header::AUTHORIZATION, &authorization)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(status.status(), StatusCode::OK);

    let command = private
        .oneshot(
            Request::post("/availability/v1/commands")
                .header(header::AUTHORIZATION, authorization)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "type": "transfer_authority",
                        "authority": "resource",
                        "new_home": "west"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(command.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn test_prepared_runtime_serves_the_metadata_frontier() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = primary_with_roster(&dir);
    let replication = config.availability.replication().unwrap().clone();
    config.availability = AvailabilityConfig::Ha(replication);
    config.dc_membership = None;
    let state = build_state(&config).unwrap();
    let public = prepare(&config, &state).await.public_routes;

    let response = public
        .oneshot(
            Request::get("/+replication/v1/frontier/resource")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}
