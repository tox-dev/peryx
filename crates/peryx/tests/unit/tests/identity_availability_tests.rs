//! Replication must not widen authorization or expose unapplied management state.
//!
//! Revocation follows [NIST RBAC]; followers fail closed per [OWASP authorization guidance].
//!
//! [NIST RBAC]: https://csrc.nist.gov/projects/role-based-access-control
//! [OWASP authorization guidance]: https://cheatsheetseries.owasp.org/cheatsheets/Authorization_Cheat_Sheet.html

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http_body_util::BodyExt as _;
use peryx_driver::state::{AppState, ServingState};
use peryx_identity::{GrantScope, Role};
use peryx_storage::meta::MetaStore;
use rstest::rstest;
use serde_json::{Value, json};
use tower::ServiceExt as _;

use crate::config::{AvailabilityConfig, Config, DcMember, DcMembership, DcRole, ReplicationConfig, SecretSource};
use crate::replication::ReplicationRuntime;
use crate::server::{build_state, router_for};

const TOKEN: &str = "replica-secret";
const PASSWORD: &str = "identity availability password";
const WRITER_IDENTITY: &str = "writer-a";

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

fn dc_replica_config(dir: &tempfile::TempDir, upstream: &str) -> Config {
    claim_writer(dir);
    Config {
        data_dir: dir.path().to_path_buf(),
        writer_identity: Some(WRITER_IDENTITY.to_owned()),
        availability: AvailabilityConfig::Dc(replica_replication(upstream)),
        ..Config::default()
    }
}

fn ha_replica_config(dir: &tempfile::TempDir, upstream: &str) -> Config {
    claim_writer(dir);
    Config {
        data_dir: dir.path().to_path_buf(),
        writer_identity: Some(WRITER_IDENTITY.to_owned()),
        availability: AvailabilityConfig::Ha(replica_replication(upstream)),
        ..Config::default()
    }
}

fn primary_replication() -> ReplicationConfig {
    ReplicationConfig::Primary {
        source: WRITER_IDENTITY.to_owned(),
        token: SecretSource::Literal(TOKEN.to_owned()),
    }
}

fn replica_replication(upstream: &str) -> ReplicationConfig {
    ReplicationConfig::Replica {
        upstream: upstream.to_owned(),
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

/// Read-only opening requires prior writer identity provisioning.
fn claim_writer(dir: &tempfile::TempDir) {
    MetaStore::open(dir.path().join("peryx.redb"))
        .unwrap()
        .claim_writer_identity(WRITER_IDENTITY)
        .unwrap();
}

async fn principal(state: &ServingState, name: &str, role: Role) -> String {
    let id = state.users.create(name).unwrap().id;
    state.users.set_password(&id, PASSWORD).await.unwrap();
    state.authorization.grant(&id, role, GrantScope::Server).unwrap();
    format!("{name}:{PASSWORD}")
}

async fn send(
    router: &Router,
    method: &str,
    path: &str,
    auth: Option<&str>,
    json: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(auth) = auth {
        request = request.header(header::AUTHORIZATION, format!("Basic {}", STANDARD.encode(auth)));
    }
    if json.is_some() {
        request = request.header(header::CONTENT_TYPE, "application/json");
    }
    let body = json.map_or_else(Body::empty, |value| Body::from(value.to_string()));
    let response = router.clone().oneshot(request.body(body).unwrap()).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes().to_vec();
    let document = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, document)
}

fn server_grant(user: &str, role: &str) -> Value {
    json!({ "user": user, "role": role, "scope": { "kind": "server" } })
}

fn writer_node(config: &Config) -> (Arc<AppState>, Router) {
    let state = build_state(config).unwrap();
    let router = router_for(state.clone());
    (state, router)
}

fn replica_node(config: &Config) -> (Arc<AppState>, Router, ReplicationRuntime) {
    let state = build_state(config).unwrap();
    let runtime = ReplicationRuntime::new(config, &state).unwrap();
    let router = runtime.mount(router_for(state.clone()));
    (state, router, runtime)
}

#[tokio::test]
async fn test_none_writer_enforces_deny_by_default_without_availability_resources() {
    let dir = tempfile::tempdir().unwrap();
    let (state, router) = writer_node(&none_config(&dir));
    let admin = principal(&state.serving, "root", Role::Administrator).await;
    let reader = principal(&state.serving, "reader", Role::RepositoryReader).await;
    let target = state.serving.users.create("grantee").unwrap().id;
    let grant = server_grant(target.as_str(), "operator");

    assert_eq!(
        send(&router, "POST", "/+grants", None, Some(grant.clone())).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        send(&router, "POST", "/+grants", Some(&reader), Some(grant.clone()))
            .await
            .0,
        StatusCode::FORBIDDEN,
    );
    assert_eq!(
        send(&router, "POST", "/+grants", Some(&admin), Some(grant)).await.0,
        StatusCode::CREATED
    );

    assert!(!state.serving.read_only);
    let (status, _) = send(&router, "GET", "/+availability/topology", Some(&admin), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[rstest]
#[case::none(none_config as fn(&tempfile::TempDir) -> Config)]
#[case::dc_writer(dc_writer_config as fn(&tempfile::TempDir) -> Config)]
#[tokio::test]
async fn test_permission_revoked_before_use_denies_the_action(#[case] build: fn(&tempfile::TempDir) -> Config) {
    let dir = tempfile::tempdir().unwrap();
    let (state, router) = writer_node(&build(&dir));
    let root = principal(&state.serving, "root", Role::Administrator).await;

    let deputy_id = state.serving.users.create("deputy").unwrap().id;
    state.serving.users.set_password(&deputy_id, PASSWORD).await.unwrap();
    let deputy = format!("deputy:{PASSWORD}");
    assert_eq!(
        send(
            &router,
            "POST",
            "/+grants",
            Some(&root),
            Some(server_grant(deputy_id.as_str(), "administrator"))
        )
        .await
        .0,
        StatusCode::CREATED,
    );

    let first = state.serving.users.create("first-grantee").unwrap().id;
    assert_eq!(
        send(
            &router,
            "POST",
            "/+grants",
            Some(&deputy),
            Some(server_grant(first.as_str(), "repository_reader"))
        )
        .await
        .0,
        StatusCode::CREATED,
    );

    assert!(
        state
            .serving
            .authorization
            .revoke(&deputy_id, Role::Administrator, &GrantScope::Server)
            .unwrap()
    );

    let second = state.serving.users.create("second-grantee").unwrap().id;
    assert_eq!(
        send(
            &router,
            "POST",
            "/+grants",
            Some(&deputy),
            Some(server_grant(second.as_str(), "repository_reader"))
        )
        .await
        .0,
        StatusCode::FORBIDDEN,
    );
    assert!(
        state.serving.authorization.grants(&second).unwrap().is_empty(),
        "a grant denied before finalization leaves no durable authority",
    );
}

#[rstest]
#[case::dc(dc_replica_config as fn(&tempfile::TempDir, &str) -> Config)]
#[case::ha(ha_replica_config as fn(&tempfile::TempDir, &str) -> Config)]
#[tokio::test]
async fn test_read_only_replica_refuses_mutations_but_preserves_read_authorization(
    #[case] build: fn(&tempfile::TempDir, &str) -> Config,
) {
    let dir = tempfile::tempdir().unwrap();
    let (state, router, _runtime) = replica_node(&build(&dir, "http://writer.invalid/"));
    assert!(state.serving.read_only, "a configured replica is read-only");
    let admin = principal(&state.serving, "root", Role::Administrator).await;

    for (path, body) in [
        ("/+grants", server_grant("someone", "operator")),
        ("/+tokens", json!({ "name": "t", "actions": ["read"] })),
    ] {
        let (status, document) = send(&router, "POST", path, Some(&admin), Some(body)).await;
        assert_eq!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{path} is refused on a replica"
        );
        assert_eq!(document["error"], "read_only_replica");
    }

    assert_eq!(
        send(&router, "GET", "/+grants", None, None).await.0,
        StatusCode::UNAUTHORIZED
    );
    let (status, status_doc) = send(&router, "GET", "/+status", Some(&admin), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(status_doc["role"], "replica");
    assert_eq!(status_doc["health"]["accepting_writes"], json!(false));
}
