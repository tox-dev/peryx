use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http_body_util::BodyExt as _;
use peryx::config::{
    AvailabilityConfig, Config, DcMember, DcMembership, DcRole, IndexConfig, IndexKind, ReplicationConfig,
    SecretSource, TokenConfig,
};
use peryx::replication::ReplicationRuntime;
use peryx::server::{build_state, router_for};
use peryx_driver::state::AppState;
use peryx_ha::{ActiveAvailabilityHandle, AvailabilityHandle};
use peryx_ha_distributed::primary_router;
use peryx_identity::{Action, GrantScope, Role};
use peryx_policy::PolicyConfig;
use peryx_storage::blob::{BlobStore, Digest};
use peryx_storage::meta::{MetaError, MetaStore};
use rstest::rstest;
use serde_json::{Value, json};
use tower::ServiceExt as _;

const MANAGED_KEY: &str = "pypi\u{0}p\u{0}hosted/secret";
const MANAGED_VALUE: &[u8] = b"managed-record";
const PASSWORD: &str = "jobs availability password";
const PROJECT: &str = "veloxdemo";
const TIGHT_LIMIT: u64 = 100;
const TOKEN: &str = "replica-secret";
const UPLOAD: &str = "s3cret";
const VERSION: &str = "1.0.0";
const WHEEL: &[u8] = include_bytes!("../../../fixtures/veloxdemo-1.0.0-py3-none-any.whl");
const WRITER_IDENTITY: &str = "writer-a";

#[rstest]
#[case::none(none_config as fn(&tempfile::TempDir) -> Config)]
#[case::dc(dc_writer_config as fn(&tempfile::TempDir) -> Config)]
#[case::ha(ha_writer_config as fn(&tempfile::TempDir) -> Config)]
#[tokio::test]
async fn test_a_quota_admits_or_refuses_a_write_in_every_mode(#[case] build: fn(&tempfile::TempDir) -> Config) {
    let dir = tempfile::tempdir().unwrap();
    let (state, router, _availability) = writer_node(&build(&dir)).await;
    let root = admin(&state).await;

    assert_eq!(upload(&router, "tight", UPLOAD).await, StatusCode::FORBIDDEN);
    assert_eq!(upload(&router, "store", UPLOAD).await, StatusCode::OK);

    let (status, quota) = send(&router, "GET", "/+quota/repository?repository=tight", Some(&root), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(quota["accounted_bytes"]["limit"], TIGHT_LIMIT);
    assert_eq!(quota["limits"]["max_accounted_bytes"], TIGHT_LIMIT);
}

#[rstest]
#[case::none(none_config as fn(&tempfile::TempDir) -> Config)]
#[case::dc(dc_writer_config as fn(&tempfile::TempDir) -> Config)]
#[case::ha(ha_writer_config as fn(&tempfile::TempDir) -> Config)]
#[tokio::test]
async fn test_retention_plans_authoritative_candidates_in_every_mode(#[case] build: fn(&tempfile::TempDir) -> Config) {
    let dir = tempfile::tempdir().unwrap();
    let (state, router, _availability) = writer_node(&build(&dir)).await;
    let root = admin(&state).await;
    let reader = reader(&state).await;
    assert_eq!(upload(&router, "store", UPLOAD).await, StatusCode::OK);

    assert_eq!(
        send(&router, "POST", "/+retention/plan", None, Some(expire_all("store")))
            .await
            .0,
        StatusCode::UNAUTHORIZED,
    );
    assert_eq!(
        send(
            &router,
            "POST",
            "/+retention/plan",
            Some(&reader),
            Some(expire_all("store")),
        )
        .await
        .0,
        StatusCode::NOT_FOUND,
    );

    let (status, plan) = send(
        &router,
        "POST",
        "/+retention/plan",
        Some(&root),
        Some(expire_all("store")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(plan["summary"]["policy_version"].is_number(), "{plan}");
    let candidate = plan["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|decision| decision["resource"] == PROJECT)
        .expect("the published project is a candidate");
    assert_eq!(candidate["group"], VERSION);
    assert_eq!(candidate["outcome"], "remove");
    assert_eq!(candidate["rule"], "resource-prefix");
}

#[tokio::test]
async fn test_the_catalog_lists_published_projects_in_every_mode() {
    for build in [
        none_config as fn(&tempfile::TempDir) -> Config,
        dc_writer_config,
        ha_writer_config,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (_state, router, _availability) = writer_node(&build(&dir)).await;

        assert!(catalog(&router).await.is_empty());
        assert_eq!(upload(&router, "store", UPLOAD).await, StatusCode::OK);
        assert!(catalog(&router).await.iter().any(|entry| entry["name"] == PROJECT));
    }
}

#[rstest]
#[case::dc(dc_replica_config as fn(&tempfile::TempDir, &str) -> Config)]
#[case::ha(ha_replica_config as fn(&tempfile::TempDir, &str) -> Config)]
#[tokio::test]
async fn test_a_read_only_replica_refuses_pypi_mutations(#[case] build: fn(&tempfile::TempDir, &str) -> Config) {
    let dir = tempfile::tempdir().unwrap();
    let (state, router, _runtime) = replica_node(&build(&dir, "http://writer.invalid/")).await;
    let root = admin(&state).await;
    assert!(state.serving.read_only);

    let (status, document) = send(
        &router,
        "POST",
        "/+repositories",
        Some(&root),
        Some(json!({"name": "another", "route": "another", "ecosystem": "pypi"})),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(document["error"], "read_only_replica");
    assert_eq!(upload(&router, "store", UPLOAD).await, StatusCode::SERVICE_UNAVAILABLE);
}

#[rstest]
#[case::dc(dc_replica_config as fn(&tempfile::TempDir, &str) -> Config)]
#[case::ha(ha_replica_config as fn(&tempfile::TempDir, &str) -> Config)]
#[tokio::test]
async fn test_a_read_only_replica_serves_a_retention_preview(#[case] build: fn(&tempfile::TempDir, &str) -> Config) {
    let dir = tempfile::tempdir().unwrap();
    let (state, router, _runtime) = replica_node(&build(&dir, "http://writer.invalid/")).await;
    let root = admin(&state).await;

    let (status, plan) = send(
        &router,
        "POST",
        "/+retention/plan",
        Some(&root),
        Some(expire_all("store")),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(plan["candidates"], json!([]));
}

#[tokio::test]
async fn test_a_replica_surfaces_pypi_state_only_within_its_frontier() {
    let (_primary_dir, upstream, listener, primary) = staged_primary().await;
    let (shutdown, stopped) = tokio::sync::oneshot::channel();
    let serve = tokio::spawn(async move {
        axum::serve(listener, primary)
            .with_graceful_shutdown(async { stopped.await.unwrap() })
            .await
            .unwrap();
    });
    let dir = tempfile::tempdir().unwrap();
    let (state, router, runtime) = replica_node(&dc_replica_config(&dir, &upstream)).await;
    let root = admin(&state).await;
    let mut applied = state.serving.replica_applied_frontier().unwrap();

    assert_eq!(state.serving.meta.current_serial().unwrap(), 0);
    assert!(state.serving.meta.get_driver_value(MANAGED_KEY).unwrap().is_none());
    let unapplied = send(
        &router,
        "POST",
        "/+retention/plan",
        Some(&root),
        Some(expire_all("store")),
    )
    .await;
    assert_eq!(unapplied.0, StatusCode::OK);
    assert_eq!(unapplied.1["candidates"], json!([]));

    let _active = AvailabilityHandle::activate(runtime).unwrap();
    applied.wait_for(|serial| *serial >= 1).await.unwrap();
    assert_eq!(state.serving.meta.current_serial().unwrap(), 1);
    assert_eq!(
        state.serving.meta.get_driver_value(MANAGED_KEY).unwrap().as_deref(),
        Some(MANAGED_VALUE),
    );

    shutdown.send(()).unwrap();
    serve.await.unwrap();
}

fn none_config(dir: &tempfile::TempDir) -> Config {
    Config {
        data_dir: dir.path().to_path_buf(),
        indexes: feature_indexes(),
        ..Config::default()
    }
}

fn dc_writer_config(dir: &tempfile::TempDir) -> Config {
    distributed_config(dir, AvailabilityConfig::Dc(primary_replication()), group())
}

fn ha_writer_config(dir: &tempfile::TempDir) -> Config {
    distributed_config(dir, AvailabilityConfig::Ha(primary_replication()), solo_group())
}

fn distributed_config(
    dir: &tempfile::TempDir,
    availability: AvailabilityConfig,
    dc_membership: DcMembership,
) -> Config {
    let node_identity = matches!(availability, AvailabilityConfig::Ha(_)).then(|| WRITER_IDENTITY.to_owned());
    Config {
        data_dir: dir.path().to_path_buf(),
        writer_identity: Some(WRITER_IDENTITY.to_owned()),
        node_identity,
        availability,
        dc_membership: Some(dc_membership),
        indexes: feature_indexes(),
        ..Config::default()
    }
}

fn dc_replica_config(dir: &tempfile::TempDir, upstream: &str) -> Config {
    replica_config(dir, upstream, AvailabilityConfig::Dc)
}

fn ha_replica_config(dir: &tempfile::TempDir, upstream: &str) -> Config {
    replica_config(dir, upstream, AvailabilityConfig::Ha)
}

fn replica_config(
    dir: &tempfile::TempDir,
    upstream: &str,
    mode: fn(ReplicationConfig) -> AvailabilityConfig,
) -> Config {
    claim_writer(dir);
    let availability = mode(replica_replication(upstream));
    let node_identity = matches!(availability, AvailabilityConfig::Ha(_)).then(|| WRITER_IDENTITY.to_owned());
    Config {
        data_dir: dir.path().to_path_buf(),
        writer_identity: Some(WRITER_IDENTITY.to_owned()),
        node_identity,
        availability,
        indexes: feature_indexes(),
        ..Config::default()
    }
}

fn feature_indexes() -> Vec<IndexConfig> {
    vec![
        hosted("store", PolicyConfig::default()),
        hosted(
            "tight",
            PolicyConfig {
                max_resource_size_bytes: Some(TIGHT_LIMIT),
                max_accounted_bytes: Some(TIGHT_LIMIT),
                ..PolicyConfig::default()
            },
        ),
    ]
}

fn hosted(name: &str, policy: PolicyConfig) -> IndexConfig {
    IndexConfig {
        name: name.to_owned(),
        route: name.to_owned(),
        ecosystem: peryx_ecosystem_pypi::ECOSYSTEM,
        kind: IndexKind::Hosted { volatile: true },
        anonymous_read: None,
        tokens: vec![writer_token()],
        policy,
        ecosystem_policy: toml::Table::new(),
        ecosystem_settings: toml::Table::new(),
        webhooks: Vec::new(),
    }
}

fn writer_token() -> TokenConfig {
    TokenConfig {
        name: "uploader".to_owned(),
        secret: SecretSource::Literal(UPLOAD.to_owned()),
        resources: vec!["*".to_owned()],
        actions: BTreeSet::from([Action::Write, Action::Delete]),
        expires_at: None,
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

fn solo_group() -> DcMembership {
    DcMembership {
        group: "east".to_owned(),
        members: vec![DcMember {
            node: WRITER_IDENTITY.to_owned(),
            dc: "east-1".to_owned(),
            address: "http://10.0.0.1:8080".to_owned(),
            role: DcRole::Writer,
        }],
    }
}

fn claim_writer(dir: &tempfile::TempDir) {
    MetaStore::open(dir.path().join("peryx.redb"))
        .unwrap()
        .claim_writer_identity(WRITER_IDENTITY)
        .unwrap();
}

async fn writer_node(config: &Config) -> (Arc<AppState>, Router, Option<Box<dyn ActiveAvailabilityHandle>>) {
    let state = build_state(config).unwrap();
    if matches!(config.availability, AvailabilityConfig::None) {
        return (state.clone(), router_for(state), None);
    }
    let listener = matches!(config.availability, AvailabilityConfig::Ha(_)).then(test_listener);
    let prepared = ReplicationRuntime::new(config, &state)
        .unwrap()
        .prepare(
            &state,
            peryx_ha_distributed::reference_inventory(
                peryx_driver::DriverSet::default().with(Arc::new(peryx_ecosystem_pypi::PypiServing)),
                state.serving.meta.clone(),
                config.indexes.iter().map(|index| index.name.clone()).collect(),
            ),
            listener,
        )
        .await
        .unwrap();
    assert_eq!(
        prepared.handle.listener_address().is_some(),
        matches!(config.availability, AvailabilityConfig::Ha(_))
    );
    let router = router_for(state.clone()).merge(prepared.public_routes);
    let active = AvailabilityHandle::activate(prepared.handle).unwrap();
    (state, router, Some(Box::new(active)))
}

struct TestListener(std::net::TcpListener);

impl peryx_ha_distributed::PreparedAvailabilityListener for TestListener {
    fn address(&self) -> std::net::SocketAddr {
        self.0.local_addr().unwrap()
    }

    fn serve(
        self: Box<Self>,
        router: Router,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Result<peryx_ha_distributed::AvailabilityListenerFuture, peryx_ha_distributed::AvailabilityListenerError> {
        self.0.set_nonblocking(true)?;
        let listener = tokio::net::TcpListener::from_std(self.0)?;
        Ok(Box::pin(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await
            .map_err(Into::into)
        }))
    }
}

fn test_listener() -> Box<dyn peryx_ha_distributed::PreparedAvailabilityListener> {
    Box::new(TestListener(std::net::TcpListener::bind("127.0.0.1:0").unwrap()))
}

async fn replica_node(config: &Config) -> (Arc<AppState>, Router, peryx_ha_distributed::DistributedHandle) {
    let state = build_state(config).unwrap();
    let runtime = ReplicationRuntime::new(config, &state).unwrap();
    let prepared = runtime
        .prepare(
            &state,
            peryx_ha_distributed::reference_inventory(
                peryx_driver::DriverSet::default().with(Arc::new(peryx_ecosystem_pypi::PypiServing)),
                state.serving.meta.clone(),
                config.indexes.iter().map(|index| index.name.clone()).collect(),
            ),
            None,
        )
        .await
        .unwrap();
    let router = router_for(state.clone()).merge(prepared.public_routes);
    (state, router, prepared.handle)
}

async fn admin(state: &AppState) -> String {
    let id = state.serving.users.create("root").unwrap().id;
    state.serving.users.set_password(&id, PASSWORD).await.unwrap();
    state
        .serving
        .authorization
        .grant(&id, Role::Administrator, GrantScope::Server)
        .unwrap();
    format!("root:{PASSWORD}")
}

async fn reader(state: &AppState) -> String {
    let id = state.serving.users.create("rita").unwrap().id;
    state.serving.users.set_password(&id, PASSWORD).await.unwrap();
    state
        .serving
        .authorization
        .grant(&id, Role::RepositoryReader, GrantScope::Server)
        .unwrap();
    format!("rita:{PASSWORD}")
}

async fn send(
    router: &Router,
    method: &str,
    path: &str,
    auth: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(auth) = auth {
        request = request.header(header::AUTHORIZATION, basic(auth));
    }
    if body.is_some() {
        request = request.header(header::CONTENT_TYPE, "application/json");
    }
    let response = router
        .clone()
        .oneshot(
            request
                .body(body.map_or_else(Body::empty, |value| Body::from(value.to_string())))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn upload(router: &Router, route: &str, secret: &str) -> StatusCode {
    let boundary = "peryxjobsboundary";
    let digest = Digest::of(WHEEL);
    let mut body = Vec::new();
    for (name, value) in [
        (":action", "file_upload"),
        ("name", PROJECT),
        ("version", VERSION),
        ("filetype", "bdist_wheel"),
        ("sha256_digest", digest.as_str()),
    ] {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"content\"; filename=\"{PROJECT}-{VERSION}-py3-none-any.whl\"\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(WHEEL);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/{route}/"))
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .header(header::AUTHORIZATION, basic(&format!("__token__:{secret}")))
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

async fn catalog(router: &Router) -> Vec<Value> {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/store/simple/")
                .header(header::ACCEPT, "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice::<Value>(&bytes).unwrap()["projects"]
        .as_array()
        .unwrap()
        .clone()
}

async fn staged_primary() -> (tempfile::TempDir, String, tokio::net::TcpListener, Router) {
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
    (dir, format!("http://{address}/"), listener, router)
}

fn expire_all(repository: &str) -> Value {
    json!({ "repository": repository, "expire": [{ "selector": "resource-prefix", "prefix": "" }] })
}

fn basic(auth: &str) -> String {
    format!("Basic {}", STANDARD.encode(auth))
}
