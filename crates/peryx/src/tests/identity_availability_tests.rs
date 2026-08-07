//! Identity and access controls exercised across the `none`, `dc`, and `ha` availability modes.
//!
//! Authorization in peryx is deny-by-default and enforced once, at the request boundary, ahead of any
//! durable mutation. These probes hold that contract as the availability mode changes: a single-node
//! `none` writer, a `dc`/`ha` writer that fronts a consensus group, and a read-only replica that
//! follows one. The point is that replication never widens a caller's reach and never exposes state a
//! node has not yet applied.
//!
//! Every arm is deterministic: requests run through the in-process router with [`ServiceExt::oneshot`],
//! and replication advances one [`sync_cycle`](crate::replication::ReplicationRuntime::sync_cycle) at a
//! time, so a "partition" is a cycle not run rather than a timed wait. No test sleeps, binds a
//! fixed port, or reads a real clock.
//!
//! The revocation case follows the [NIST RBAC] model - a permission removed before an action is used
//! denies it - and the replica cases follow [OWASP authorization guidance]: a follower fails closed on
//! management state past its readable frontier and refuses every mutation.
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
use peryx_driver::state::AppState;
use peryx_ha_distributed::primary_router;
use peryx_identity::{GrantScope, Role};
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::{MetaError, MetaStore};
use rstest::rstest;
use serde_json::{Value, json};
use tower::ServiceExt as _;

use crate::config::{AvailabilityConfig, Config, DcMember, DcMembership, DcRole, ReplicationConfig, SecretSource};
use crate::replication::ReplicationRuntime;
use crate::server::{build_state, router_for};

/// The shared secret a replica presents to its primary; irrelevant to the assertions but required to
/// build the runtime.
const TOKEN: &str = "replica-secret";
/// The password every seeded principal authenticates with. A constant keeps the Basic header trivial.
const PASSWORD: &str = "identity availability password";
/// The writer identity a `dc`/`ha` store is claimed under, so a replica's configured writer agrees
/// with the metadata store it follows.
const WRITER_IDENTITY: &str = "writer-a";
/// A journaled management record the primary holds and a partitioned replica must not surface until it
/// has applied the page that carries it.
const MANAGED_KEY: &str = "pypi\u{0}p\u{0}hosted/secret";
const MANAGED_VALUE: &[u8] = b"managed-record";

/// A single-node `none` writer: the current default posture, with no availability resource.
fn none_config(dir: &tempfile::TempDir) -> Config {
    Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default()
    }
}

/// A `dc` writer (primary) that fronts a two-member group. It accepts mutations and, unlike `none`,
/// projects an availability topology.
fn dc_writer_config(dir: &tempfile::TempDir) -> Config {
    Config {
        data_dir: dir.path().to_path_buf(),
        writer_identity: Some(WRITER_IDENTITY.to_owned()),
        availability: AvailabilityConfig::Dc(primary_replication()),
        dc_membership: Some(group()),
        ..Config::default()
    }
}

/// A `dc` replica that follows `upstream`. A configured replica is read-only, so its router refuses
/// mutations.
fn dc_replica_config(dir: &tempfile::TempDir, upstream: &str) -> Config {
    claim_writer(dir);
    Config {
        data_dir: dir.path().to_path_buf(),
        writer_identity: Some(WRITER_IDENTITY.to_owned()),
        availability: AvailabilityConfig::Dc(replica_replication(upstream)),
        ..Config::default()
    }
}

/// An `ha` replica: the same read-only follower posture as `dc`, reached through an `ha` roster so the
/// mode label differs.
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

/// Seed the replica store's writer identity before it is opened read-only, the way a provisioned
/// replica is handed the writer it will follow.
fn claim_writer(dir: &tempfile::TempDir) {
    MetaStore::open(dir.path().join("peryx.redb"))
        .unwrap()
        .claim_writer_identity(WRITER_IDENTITY)
        .unwrap();
}

/// Create a principal holding `role` over the whole server and return its Basic `user:password`
/// credential, authenticated by display name.
async fn principal(state: &AppState, name: &str, role: Role) -> String {
    let id = state.users.create(name).unwrap().id;
    state.users.set_password(&id, PASSWORD).await.unwrap();
    state.authorization.grant(&id, role, GrantScope::Server).unwrap();
    format!("{name}:{PASSWORD}")
}

/// One request through the router. `auth` is a Basic `user:password` pair; `json` sets a JSON body and
/// content type, which the management mutations require.
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

/// A `POST /+grants` body granting `role` over the whole server to `user`.
fn server_grant(user: &str, role: &str) -> Value {
    json!({ "user": user, "role": role, "scope": { "kind": "server" } })
}

/// Build a writer node's state and neutral router from `config`. The writer accepts mutations, so its
/// router carries no read-only gate.
fn writer_node(config: &Config) -> (Arc<AppState>, Router) {
    let state = build_state(config).unwrap();
    let router = router_for(state.clone());
    (state, router)
}

/// Build a read-only replica's state and full router (with its mutation gate) plus the runtime that
/// drives its sync.
fn replica_node(config: &Config) -> (Arc<AppState>, Router, ReplicationRuntime) {
    let state = build_state(config).unwrap();
    let runtime = ReplicationRuntime::new(config, &state).unwrap();
    let router = runtime.mount(router_for(state.clone()));
    (state, router, runtime)
}

/// A `none` writer refuses management mutations to an anonymous or unprivileged caller and admits only
/// the administrator, and it stands up none of the availability surface. This is the baseline the
/// availability modes must not weaken.
#[tokio::test]
async fn test_none_writer_enforces_deny_by_default_without_availability_resources() {
    let dir = tempfile::tempdir().unwrap();
    let (state, router) = writer_node(&none_config(&dir));
    let admin = principal(&state, "root", Role::Administrator).await;
    let reader = principal(&state, "reader", Role::RepositoryReader).await;
    let target = state.users.create("grantee").unwrap().id;
    let grant = server_grant(target.as_str(), "operator");

    // Deny-by-default at the boundary: no credential is unauthorized, an unprivileged one is forbidden.
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
    // Only the administrator's grant is admitted.
    assert_eq!(
        send(&router, "POST", "/+grants", Some(&admin), Some(grant)).await.0,
        StatusCode::CREATED
    );

    // `none` builds no availability resource: it accepts writes, runs no consensus control plane, and
    // reports the single-node topology.
    assert!(!state.read_only);
    assert!(state.control_plane().is_none());
    let (status, topology) = send(&router, "GET", "/+availability/topology", Some(&admin), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(topology["mode"], "none");
}

/// A permission revoked before it is used denies the action, on a `none` writer and on a `dc` writer
/// alike. The revoked administrator's second grant is refused at the boundary and never reaches the
/// store, so replication of the writer does not weaken revocation. Follows the NIST RBAC model.
#[rstest]
#[case::none(none_config as fn(&tempfile::TempDir) -> Config)]
#[case::dc_writer(dc_writer_config as fn(&tempfile::TempDir) -> Config)]
#[tokio::test]
async fn test_permission_revoked_before_use_denies_the_action(#[case] build: fn(&tempfile::TempDir) -> Config) {
    let dir = tempfile::tempdir().unwrap();
    let (state, router) = writer_node(&build(&dir));
    let root = principal(&state, "root", Role::Administrator).await;

    // Root delegates administration to a deputy, who then holds the authority to grant.
    let deputy_id = state.users.create("deputy").unwrap().id;
    state.users.set_password(&deputy_id, PASSWORD).await.unwrap();
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

    // While the delegation stands, the deputy can grant.
    let first = state.users.create("first-grantee").unwrap().id;
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

    // Root revokes the deputy's administration.
    assert!(
        state
            .authorization
            .revoke(&deputy_id, Role::Administrator, &GrantScope::Server)
            .unwrap()
    );

    // The next grant the deputy attempts is denied at the boundary and is never recorded.
    let second = state.users.create("second-grantee").unwrap().id;
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
        state.authorization.grants(&second).unwrap().is_empty(),
        "a grant denied before finalization leaves no durable authority",
    );
}

/// A read-only replica refuses every identity mutation with `503 read_only_replica` yet still serves
/// reads under the same deny-by-default authorization as a writer. Holds for `dc` and `ha` followers.
#[rstest]
#[case::dc(dc_replica_config as fn(&tempfile::TempDir, &str) -> Config)]
#[case::ha(ha_replica_config as fn(&tempfile::TempDir, &str) -> Config)]
#[tokio::test]
async fn test_read_only_replica_refuses_mutations_but_preserves_read_authorization(
    #[case] build: fn(&tempfile::TempDir, &str) -> Config,
) {
    let dir = tempfile::tempdir().unwrap();
    let (state, router, _runtime) = replica_node(&build(&dir, "http://writer.invalid/"));
    assert!(state.read_only, "a configured replica is read-only");
    let admin = principal(&state, "root", Role::Administrator).await;

    // Every mutation is refused by the replica gate ahead of the handler, whatever the credential.
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

    // Reads still flow, and still fail closed for an anonymous caller: replication did not relax authz.
    assert_eq!(
        send(&router, "GET", "/+grants", None, None).await.0,
        StatusCode::UNAUTHORIZED
    );
    let (status, status_doc) = send(&router, "GET", "/+status", Some(&admin), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(status_doc["role"], "replica");
    assert_eq!(status_doc["health"]["accepting_writes"], json!(false));
}

/// Stand up a primary whose journal already carries [`MANAGED_KEY`], served over an in-process
/// listener a replica can follow. The returned directory keeps the primary's stores alive.
async fn start_primary() -> (tempfile::TempDir, String, tokio::task::JoinHandle<()>) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    meta.commit_driver_txn(|txn| {
        txn.put(MANAGED_KEY, MANAGED_VALUE)?;
        Ok::<_, MetaError>(((), vec![b"managed".to_vec()]))
    })
    .unwrap();
    let router = primary_router(WRITER_IDENTITY, TOKEN, meta, blobs).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (dir, format!("http://{address}/"), task)
}

/// A partitioned replica exposes only the serial it has applied and never the primary's later state,
/// and refuses mutations throughout; healing the partition with one sync cycle brings the state within
/// its readable frontier. Follows OWASP authorization guidance that a follower fails closed on state
/// past its frontier.
#[tokio::test]
async fn test_replica_serves_only_state_within_its_readable_frontier() {
    let (_primary_dir, upstream, serve) = start_primary().await;
    let dir = tempfile::tempdir().unwrap();
    let (state, router, mut runtime) = replica_node(&dc_replica_config(&dir, &upstream));
    let admin = principal(&state, "root", Role::Administrator).await;

    // Partitioned: no cycle has run, so the replica holds nothing past serial zero and cannot see the
    // primary's managed record. It fails closed rather than serving state it has not applied.
    assert_eq!(state.meta.current_serial().unwrap(), 0);
    assert!(state.meta.get_driver_value(MANAGED_KEY).unwrap().is_none());
    let (status, status_doc) = send(&router, "GET", "/+status", Some(&admin), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(status_doc["health"]["accepting_writes"], json!(false));
    // And it refuses to originate the state a partition withholds.
    let refused = send(
        &router,
        "POST",
        "/+grants",
        Some(&admin),
        Some(server_grant("x", "operator")),
    )
    .await;
    assert_eq!(refused.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(refused.1["error"], "read_only_replica");

    // Heal the partition: one cycle applies the page, moving the primary's state within the frontier.
    assert_eq!(runtime.sync_cycle().await, Some(true));
    assert_eq!(state.meta.current_serial().unwrap(), 1);
    assert_eq!(
        state.meta.get_driver_value(MANAGED_KEY).unwrap().as_deref(),
        Some(MANAGED_VALUE),
    );

    serve.abort();
}
