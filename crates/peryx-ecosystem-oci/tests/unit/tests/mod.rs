mod authority;
mod bearer_tests;
mod conditional_blob_tests;
mod conditional_manifest_tests;
mod conformance_tests;
mod contents_tests;
mod discovery_tests;
mod frontier;
mod manifest_schema_tests;
mod metrics_tests;
mod mirror_contract_tests;
mod mirror_tests;
mod negotiate_tests;
mod outbox_tests;
mod plugin_contract_tests;
mod policy_tests;
mod property_tests;
mod push_tests;
mod quota_tests;
mod replication_tests;
mod revocation_tests;
mod scope_tests;
mod search_tests;
mod serve;
mod tag_name_tests;
mod upload_session_tests;
mod virtual_tests;
mod web_tests;
mod webhooks_tests;

use std::collections::HashMap;
use std::future::{Future, poll_fn};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::task::Poll;

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use bytes::Bytes;
use http_body_util::BodyExt as _;
use peryx_core::Ecosystem;
use peryx_driver::AppState;
use peryx_driver::rate_limit::RateLimitConfig;
use peryx_http::router;
use peryx_identity::{Action, Glob, Grant, IndexAcl, NamedToken, Signer};
use peryx_index::{Index, IndexKind};
use peryx_policy::Policy;
use peryx_storage::blob::{BlobStore, Digest};
use peryx_storage::meta::MetaStore;
use peryx_upstream::UpstreamClient;
use rstest::rstest;
use tempfile::TempDir;
use tower::ServiceExt as _;

use crate::IndexSettings;

fn bind_ownership(state: &AppState, ownership: Arc<dyn peryx_ha::OwnershipAuthority>) {
    state
        .serving
        .plugin_service::<TestOwnership>()
        .expect("distributed test ownership is installed")
        .bind(ownership);
}

#[derive(Default)]
struct TestOwnership(RwLock<Option<Arc<dyn peryx_ha::OwnershipAuthority>>>);

impl TestOwnership {
    fn bind(&self, ownership: Arc<dyn peryx_ha::OwnershipAuthority>) {
        *self.0.write().unwrap() = Some(ownership);
    }

    fn ownership(&self) -> Option<Arc<dyn peryx_ha::OwnershipAuthority>> {
        self.0.read().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl peryx_ha::OwnershipAuthority for TestOwnership {
    async fn claim_home(&self, authority: &str) -> Result<peryx_ha::HomeClaim, peryx_ha::OwnershipError> {
        match self.ownership() {
            Some(owner) => owner.claim_home(authority).await,
            None => Ok(peryx_ha::HomeClaim {
                home: "local".to_owned(),
                epoch: 0,
            }),
        }
    }

    fn cluster_status(&self) -> peryx_ha::ClusterStatus {
        self.ownership().map_or(
            peryx_ha::ClusterStatus {
                leader: None,
                term: 0,
                voters: Vec::new(),
            },
            |owner| owner.cluster_status(),
        )
    }

    async fn committed_epoch(&self, authority: &str) -> u64 {
        match self.ownership() {
            Some(owner) => owner.committed_epoch(authority).await,
            None => 0,
        }
    }

    async fn admit_epoch(&self, authority: &str, presented: u64) -> bool {
        match self.ownership() {
            Some(owner) => owner.admit_epoch(authority, presented).await,
            None => true,
        }
    }

    async fn begin_epoch_write(
        &self,
        authority: &str,
        presented: u64,
    ) -> Result<Option<peryx_ha::AuthorityWriteLease>, peryx_ha::OwnershipError> {
        match self.ownership() {
            Some(owner) => owner.begin_epoch_write(authority, presented).await,
            None => Ok(Some(peryx_ha::AuthorityWriteLease {
                authority: authority.to_owned(),
                epoch: presented,
                id: "local-write".to_owned(),
                expires_at_unix: i64::MAX,
            })),
        }
    }

    async fn finish_epoch_write(&self, lease: &peryx_ha::AuthorityWriteLease) -> Result<(), peryx_ha::OwnershipError> {
        match self.ownership() {
            Some(owner) => owner.finish_epoch_write(lease).await,
            None => Ok(()),
        }
    }

    async fn transfer_home(
        &self,
        authority: &str,
        new_home: &str,
    ) -> Result<Option<peryx_ha::TransferOutcome>, peryx_ha::OwnershipError> {
        match self.ownership() {
            Some(owner) => owner.transfer_home(authority, new_home).await,
            None => Ok(None),
        }
    }
}

#[derive(Clone)]
pub struct ResponseGate {
    entered: Arc<tokio::sync::Semaphore>,
    released: Arc<(Mutex<bool>, Condvar)>,
}

impl ResponseGate {
    fn new() -> Self {
        Self {
            entered: Arc::new(tokio::sync::Semaphore::new(0)),
            released: Arc::new((Mutex::new(false), Condvar::new())),
        }
    }

    pub(crate) async fn entered(&self) -> ResponseRelease {
        self.entered
            .acquire()
            .await
            .expect("response gate remains open")
            .forget();
        ResponseRelease(self.clone())
    }

    fn release(&self) {
        let (released, wake) = &*self.released;
        *released.lock().expect("response gate is never poisoned") = true;
        wake.notify_all();
    }

    pub(crate) fn block(&self) {
        self.entered.add_permits(1);
        let (released, wake) = &*self.released;
        let _released = wake
            .wait_while(released.lock().expect("response gate is never poisoned"), |released| {
                !*released
            })
            .expect("response gate is never poisoned");
    }
}

#[must_use]
pub struct ResponseRelease(ResponseGate);

impl Drop for ResponseRelease {
    fn drop(&mut self) {
        self.0.release();
    }
}

struct GatedResponse {
    gate: ResponseGate,
    response: wiremock::ResponseTemplate,
}

impl wiremock::Respond for GatedResponse {
    fn respond(&self, _request: &wiremock::Request) -> wiremock::ResponseTemplate {
        self.gate.block();
        self.response.clone()
    }
}

pub fn response_gate() -> ResponseGate {
    ResponseGate::new()
}

pub fn gated_response(response: wiremock::ResponseTemplate) -> (ResponseGate, impl wiremock::Respond) {
    let gate = ResponseGate::new();
    (gate.clone(), GatedResponse { gate, response })
}

pub fn observe_pending<F>(future: F) -> (tokio::task::JoinHandle<F::Output>, tokio::sync::oneshot::Receiver<()>)
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let (pending_tx, pending_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let mut future = Box::pin(future);
        let mut pending_tx = Some(pending_tx);
        poll_fn(move |context| match future.as_mut().poll(context) {
            Poll::Ready(output) => Poll::Ready(output),
            Poll::Pending => {
                if let Some(pending_tx) = pending_tx.take() {
                    let _ = pending_tx.send(());
                }
                Poll::Pending
            }
        })
        .await
    });
    (task, pending_rx)
}

fn writer_acl(secret: impl Into<String>) -> IndexAcl {
    IndexAcl {
        anonymous_read: true,
        tokens: vec![NamedToken {
            name: "uploader".to_owned(),
            secret: secret.into(),
            grants: vec![Grant {
                resources: vec![Glob::new("*")],
                actions: std::collections::BTreeSet::from([Action::Write, Action::Delete]),
            }],
            expires_at: None,
        }],
    }
}

fn app_with(dir: &TempDir, index: Index) -> (Arc<AppState>, axum::Router) {
    app_with_indexes(dir, vec![index])
}

fn app_with_indexes(dir: &TempDir, indexes: Vec<Index>) -> (Arc<AppState>, axum::Router) {
    app_with_journal(dir, indexes, false)
}

fn app_with_journal(dir: &TempDir, indexes: Vec<Index>, journal: bool) -> (Arc<AppState>, axum::Router) {
    app_with_setup(dir, indexes, journal, |_| {})
}

fn app_with_distributed(dir: &TempDir, index: Index) -> (Arc<AppState>, axum::Router) {
    app_with_setup(dir, vec![index], false, |state| {
        install_test_distributed(state, None);
    })
}

fn app_with_setup(
    dir: &TempDir,
    indexes: Vec<Index>,
    journal: bool,
    setup: impl FnOnce(&mut AppState),
) -> (Arc<AppState>, axum::Router) {
    let has_oci_index = indexes.iter().any(|index| index.ecosystem == crate::ECOSYSTEM);
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let mut state = AppState::with_clock(meta, blobs, 60, indexes, Arc::new(|| 1000));
    if has_oci_index {
        install_oci(&mut state, HashMap::new(), journal);
    }
    setup(&mut state);
    let state = Arc::new(state);
    (state.clone(), router(state))
}

fn install_oci(state: &mut AppState, settings: HashMap<String, IndexSettings>, distributed: bool) {
    let settings: HashMap<String, peryx_driver::serving::CompiledEcosystemSettings> = settings
        .into_iter()
        .map(|(name, settings)| {
            (
                name,
                peryx_driver::serving::CompiledEcosystemSettings::new(crate::ECOSYSTEM, settings),
            )
        })
        .collect();
    let registry = peryx_plugin_registry::PluginRegistry::new(vec![crate::registration()])
        .unwrap()
        .activate([crate::ECOSYSTEM])
        .unwrap();
    registry.register_activated_capabilities(&mut state.capability_install_context());
    if distributed {
        registry
            .install_distributed_drivers(&mut state.distributed_install_context().unwrap(), &settings)
            .unwrap();
    } else {
        registry
            .install_drivers(&mut state.runtime_install_context().unwrap(), &settings)
            .unwrap();
    }
}

fn install_test_distributed(state: &mut AppState, availability: Option<Arc<dyn peryx_ha::BlobAvailability>>) {
    let ownership = Arc::new(TestOwnership::default());
    state
        .install_distributed_availability(peryx_ha::AvailabilityStateInstall {
            role: peryx_core::NodeRole::Writer,
            topology: local_topology(),
            blobs: peryx_ha::BlobServices::new(availability, Arc::new(LocalDurability)),
            analytics: Arc::new(UnavailableCompleteness),
            capabilities: peryx_ha::AvailabilityCapabilities {
                ownership: Some(ownership.clone()),
                ..Default::default()
            },
            authority_drainer: None,
            operations: None,
        })
        .unwrap();
    state.register_plugin_service(ownership).unwrap();
}

fn local_topology() -> peryx_core::TopologyConfig {
    peryx_core::TopologyConfig {
        mode: peryx_core::TopologyMode::Ha,
        group: Some("test".to_owned()),
        members: vec![peryx_core::TopologyMember {
            node: "writer".to_owned(),
            dc: "local".to_owned(),
            address: "http://127.0.0.1".to_owned(),
            role: peryx_core::NodeRole::Writer,
        }],
        local_node: Some("writer".to_owned()),
    }
}

struct LocalDurability;

#[async_trait::async_trait]
impl peryx_ha::BlobWriteDurability for LocalDurability {
    async fn confirm(&self, write: peryx_ha::CommittedBlob<'_>) -> peryx_ha::WriteDurability {
        peryx_ha::WriteDurability::Confirmed {
            scope: write.evidence().scope(),
        }
    }
}

struct UnavailableCompleteness;

impl peryx_ha::AnalyticsCompleteness for UnavailableCompleteness {
    fn assess(
        &self,
        _meta: &dyn peryx_ha::AnalyticsSnapshotStore,
        _expected: &[peryx_ha::ExpectedProducer],
        _query: &peryx_ha::CompletenessQuery,
    ) -> Result<peryx_ha::CompletenessReport, peryx_ha::CompletenessError> {
        Err(peryx_ha::CompletenessError)
    }
}

fn replica_router(state: &Arc<AppState>, indexes: Vec<Index>) -> (Arc<AppState>, axum::Router) {
    let mut replica = AppState::with_clock(
        state.serving.meta.clone(),
        state.serving.blobs.clone(),
        60,
        indexes,
        Arc::new(|| 1000),
    );
    install_oci(&mut replica, HashMap::new(), false);
    replica.set_read_only(true).unwrap();
    let replica = Arc::new(replica);
    (replica.clone(), router(replica))
}

fn oci_index(name: &str, route: &str, kind: IndexKind) -> Index {
    Index {
        name: name.to_owned(),
        route: route.to_owned(),
        ecosystem: crate::ECOSYSTEM,
        kind,
        policy: Policy::default(),
        acl: IndexAcl::default(),
    }
}

fn writable_index(name: &str, route: &str, volatile: bool, token: &str) -> Index {
    Index {
        acl: writer_acl(token),
        ..oci_index(name, route, IndexKind::Hosted { volatile })
    }
}

/// `jsonwebtoken` validates expiry against wall time.
fn realm_app(dir: &TempDir, indexes: Vec<Index>) -> (Arc<AppState>, axum::Router) {
    realm_app_with_clock_and_limits(
        dir,
        indexes,
        Arc::new(current_unix_time),
        RateLimitConfig::default(),
        300,
    )
}

fn realm_app_with_clock_and_limits(
    dir: &TempDir,
    indexes: Vec<Index>,
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    rate_limit: RateLimitConfig,
    token_ttl_secs: i64,
) -> (Arc<AppState>, axum::Router) {
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let mut state = AppState::with_limits(
        meta,
        blobs,
        60,
        indexes,
        clock,
        rate_limit,
        Vec::<(String, usize)>::new(),
    );
    install_oci(&mut state, HashMap::new(), false);
    state
        .set_token_realm(
            Signer::new(b"realm-test-signing-key", crate::TOKEN_SERVICE),
            token_ttl_secs,
        )
        .unwrap();
    let state = Arc::new(state);
    (state.clone(), router(state))
}

fn current_unix_time() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
}

fn scoped_index(name: &str, route: &str, token: &str, secret: &str, glob: &str, actions: &[Action]) -> Index {
    Index {
        acl: IndexAcl {
            anonymous_read: false,
            tokens: vec![NamedToken {
                name: token.to_owned(),
                secret: secret.to_owned(),
                grants: vec![Grant {
                    resources: vec![Glob::new(glob)],
                    actions: actions.iter().copied().collect(),
                }],
                expires_at: None,
            }],
        },
        ..oci_index(name, route, IndexKind::Hosted { volatile: true })
    }
}

fn token_from(body: &Bytes) -> String {
    serde_json::from_slice::<serde_json::Value>(body).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn proxy(dir: &TempDir, upstream: &str, offline: bool) -> (Arc<AppState>, axum::Router) {
    let client = UpstreamClient::new(upstream).unwrap();
    app_with(dir, oci_index("hub", "hub", IndexKind::Cached { client, offline }))
}

/// Shared bytes must not share repository authorization.
fn proxy_pair(dir: &TempDir, up_a: &str, up_b: &str) -> (Arc<AppState>, axum::Router) {
    let cached = |upstream: &str| IndexKind::Cached {
        client: UpstreamClient::new(upstream).unwrap(),
        offline: false,
    };
    app_with_indexes(
        dir,
        vec![
            oci_index("hub", "hub", cached(up_a)),
            oci_index("vault", "vault", cached(up_b)),
        ],
    )
}

fn proxy_with_settings(dir: &TempDir, upstream: &str, settings: IndexSettings) -> (Arc<AppState>, axum::Router) {
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let client = UpstreamClient::new(upstream).unwrap();
    let index = oci_index("hub", "hub", IndexKind::Cached { client, offline: false });
    let mut state = AppState::with_clock(meta, blobs, 60, vec![index], Arc::new(|| 1000));
    install_oci(&mut state, HashMap::from([("hub".to_owned(), settings)]), false);
    let state = Arc::new(state);
    (state.clone(), router(state))
}

fn proxy_with_auth(dir: &TempDir, upstream: &str, auth: peryx_upstream::Auth) -> (Arc<AppState>, axum::Router) {
    let client = UpstreamClient::with_auth(upstream, auth).unwrap();
    app_with(
        dir,
        oci_index("hub", "hub", IndexKind::Cached { client, offline: false }),
    )
}

fn proxy_with_clock(
    dir: &TempDir,
    upstream: &str,
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
) -> (Arc<AppState>, axum::Router) {
    proxy_with_stale(dir, upstream, clock, peryx_driver::DEFAULT_MAX_STALE_SECS)
}

fn proxy_with_stale(
    dir: &TempDir,
    upstream: &str,
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    max_stale_secs: i64,
) -> (Arc<AppState>, axum::Router) {
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let index = oci_index(
        "hub",
        "hub",
        IndexKind::Cached {
            client: UpstreamClient::new(upstream).unwrap(),
            offline: false,
        },
    );
    let mut state = AppState::with_clock(meta, blobs, 60, vec![index], clock);
    Arc::get_mut(&mut state.serving).unwrap().max_stale_secs = max_stale_secs;
    install_oci(&mut state, HashMap::new(), false);
    let state = Arc::new(state);
    (state.clone(), router(state))
}

fn hosted(dir: &TempDir) -> (Arc<AppState>, axum::Router) {
    app_with(dir, oci_index("store", "store", IndexKind::Hosted { volatile: false }))
}

fn hosted_writable(dir: &TempDir, token: &str) -> (Arc<AppState>, axum::Router) {
    app_with(dir, writable_index("store", "store", true, token))
}

fn hosted_writable_distributed(dir: &TempDir, token: &str) -> (Arc<AppState>, axum::Router) {
    app_with_distributed(dir, writable_index("store", "store", true, token))
}

fn hosted_writable_distributed_with_clock(
    dir: &TempDir,
    token: &str,
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
) -> (Arc<AppState>, axum::Router) {
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let mut state = AppState::with_clock(
        meta,
        blobs,
        60,
        vec![writable_index("store", "store", true, token)],
        clock,
    );
    install_oci(&mut state, HashMap::new(), false);
    install_test_distributed(&mut state, None);
    let state = Arc::new(state);
    (state.clone(), router(state))
}

fn hosted_with_clock(
    dir: &TempDir,
    token: &str,
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
) -> (Arc<AppState>, axum::Router) {
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let index = writable_index("store", "store", true, token);
    let mut state = AppState::with_clock(meta, blobs, 60, vec![index], clock);
    install_oci(&mut state, HashMap::new(), false);
    let state = Arc::new(state);
    (state.clone(), router(state))
}

fn virtual_stack(dir: &TempDir, upstream: &str) -> (Arc<AppState>, axum::Router) {
    let client = UpstreamClient::new(upstream).unwrap();
    app_with_indexes(
        dir,
        vec![
            writable_index("images", "images", true, "s3cret"),
            oci_index("hub", "hub", IndexKind::Cached { client, offline: false }),
            oci_index(
                "reg",
                "reg",
                IndexKind::Virtual {
                    layers: vec![0, 1],
                    write_target: Some(0),
                },
            ),
        ],
    )
}

fn auth(token: &str) -> String {
    use base64::Engine as _;
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("_:{token}"))
    )
}

async fn send_body(
    app: &axum::Router,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> (StatusCode, HeaderMap, Bytes) {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, body)
}

async fn send(app: &axum::Router, method: Method, uri: &str) -> (StatusCode, HeaderMap, Bytes) {
    send_with(app, method, uri, &[]).await
}

async fn search_total(app: &axum::Router, query: &str) -> u64 {
    let response = send(app, Method::GET, &format!("/+search?q={query}&page_size=25")).await;
    assert_eq!(response.0, StatusCode::OK);
    serde_json::from_slice::<serde_json::Value>(&response.2).unwrap()["total"]
        .as_u64()
        .unwrap()
}

async fn send_with(
    app: &axum::Router,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, HeaderMap, Bytes) {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let response = app.clone().oneshot(builder.body(Body::empty()).unwrap()).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, body)
}

fn assert_registry_version(headers: &HeaderMap) {
    assert_eq!(headers["docker-distribution-api-version"], "registry/2.0");
}

#[tokio::test]
async fn test_version_check_confirms_a_v2_registry() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, "http://127.0.0.1:1/", false);
    let (status, headers, _) = send(&app, Method::GET, "/v2/").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["docker-distribution-api-version"], "registry/2.0");
}

#[tokio::test]
async fn test_version_check_answers_head() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, "http://127.0.0.1:1/", false);
    let (status, _, _) = send(&app, Method::HEAD, "/v2/").await;
    assert_eq!(status, StatusCode::OK);
}

#[rstest]
#[case::version("/v2/")]
#[case::token("/v2/token?service=peryx")]
#[tokio::test]
async fn test_oci_routes_are_not_found_without_an_oci_index(#[case] path: &str) {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = app_with_setup(
        &dir,
        vec![Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: Ecosystem::new("other"),
            kind: IndexKind::Hosted { volatile: false },
            policy: Policy::default(),
            acl: IndexAcl::default(),
        }],
        false,
        |state| {
            state
                .set_token_realm(Signer::new(b"signing-key", crate::TOKEN_SERVICE), 1)
                .unwrap();
        },
    );
    let (status, _, body) = send(&app, Method::GET, path).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, Bytes::from_static(b"not found"));
}

#[tokio::test]
async fn test_writing_to_a_proxy_index_is_denied() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, "http://127.0.0.1:1/", false);
    let (status, _, body) = send(&app, Method::PUT, "/v2/hub/app/manifests/latest").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body_has_code(&body, "DENIED"), "{body:?}");
}

#[rstest]
#[case::manifest("/v2/hub/app/manifests/latest", "GET, HEAD, PUT, DELETE")]
#[case::upload_session("/v2/hub/app/blobs/uploads/session", "GET, PATCH, PUT, DELETE")]
#[tokio::test]
async fn test_unsupported_method_on_a_route_includes_allow(#[case] path: &str, #[case] allow: &str) {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, "http://127.0.0.1:1/", false);
    let (status, headers, body) = send(&app, Method::POST, path).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(headers[header::ALLOW], allow);
    assert!(body_has_code(&body, "UNSUPPORTED"), "{body:?}");
}

#[tokio::test]
async fn test_unknown_route_reports_name_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, "http://127.0.0.1:1/", false);
    let (status, headers, body) = send(&app, Method::GET, "/v2/hub/app/frobnicate/x").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_registry_version(&headers);
    assert!(body_has_code(&body, "NAME_UNKNOWN"), "{body:?}");
}

#[rstest]
#[case::anonymous(None, None)]
#[case::basic(Some("Basic invalid-one"), Some("Basic invalid-two"))]
#[case::bearer(Some("Bearer invalid-one"), Some("Bearer invalid-two"))]
#[tokio::test]
async fn test_v2_anonymous_and_invalid_credentials_share_the_ip_bucket(
    #[case] first_credential: Option<&str>,
    #[case] second_credential: Option<&str>,
) {
    let dir = tempfile::tempdir().unwrap();
    let app = rate_limited_oci_app(&dir);
    let first_status = rate_limited_manifest_read(&app, first_credential).await;
    let second_status = rate_limited_manifest_read(&app, second_credential).await;

    assert_eq!(
        (first_status, second_status),
        (StatusCode::NOT_FOUND, StatusCode::TOO_MANY_REQUESTS)
    );
}

fn rate_limited_oci_app(dir: &TempDir) -> axum::Router {
    use peryx_driver::rate_limit::{RateLimitConfig, RouteLimit};

    let mut state = AppState::with_limits(
        MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        BlobStore::new(dir.path().join("blobs")),
        60,
        vec![oci_index("store", "store", IndexKind::Hosted { volatile: false })],
        Arc::new(|| 1000),
        RateLimitConfig {
            listing: RouteLimit::new(1, 60),
            ..RateLimitConfig::enabled_defaults()
        },
        Vec::<(String, usize)>::new(),
    );
    install_oci(&mut state, HashMap::new(), false);
    router(Arc::new(state))
}

async fn rate_limited_manifest_read(app: &axum::Router, credential: Option<&str>) -> StatusCode {
    let mut headers = vec![("x-forwarded-for", "192.0.2.9")];
    if let Some(credential) = credential {
        headers.push(("authorization", credential));
    }
    send_with(app, Method::GET, "/v2/store/app/manifests/1.0", &headers)
        .await
        .0
}

fn body_has_code(body: &Bytes, code: &str) -> bool {
    let text = std::str::from_utf8(body).unwrap_or("");
    text.contains(&format!("\"{code}\""))
}

fn oci_digest(bytes: &[u8]) -> String {
    format!("sha256:{}", Digest::of(bytes).as_str())
}

/// The config blob a fixture image manifest names. A push validates the manifest document and then
/// checks that every blob it names is one this repository holds, so a valid image fixture uploads it.
const CONFIG_BLOB: &[u8] = br#"{"architecture":"amd64","os":"linux"}"#;

/// A schema-valid image manifest under `media_type` naming [`CONFIG_BLOB`], with `extra` appended as
/// further top-level fields.
fn image_manifest(media_type: &str, extra: &str) -> Vec<u8> {
    format!(
        r#"{{"schemaVersion":2,"mediaType":"{media_type}","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{}","size":{}}},"layers":[]{extra}}}"#,
        oci_digest(CONFIG_BLOB),
        CONFIG_BLOB.len(),
    )
    .into_bytes()
}

/// Upload [`CONFIG_BLOB`] into `name`, so an image manifest naming it may be pushed there.
async fn seed_config(app: &axum::Router, name: &str, authorization: &str) {
    let (status, ..) = send_body(
        app,
        Method::POST,
        &format!("/v2/{name}/blobs/uploads/?digest={}", oci_digest(CONFIG_BLOB)),
        &[("authorization", authorization)],
        CONFIG_BLOB.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
}

/// The fake fences uploads when its leased and current epochs differ.
struct EpochAuthority {
    committed: std::sync::atomic::AtomicU64,
    current: std::sync::atomic::AtomicU64,
    entered: Option<Arc<tokio::sync::Semaphore>>,
    available: bool,
}

impl EpochAuthority {
    fn settled(epoch: u64) -> Arc<Self> {
        Arc::new(Self {
            committed: std::sync::atomic::AtomicU64::new(epoch),
            current: std::sync::atomic::AtomicU64::new(epoch),
            entered: None,
            available: true,
        })
    }

    fn superseded(leased: u64, current: u64) -> Arc<Self> {
        Arc::new(Self {
            committed: std::sync::atomic::AtomicU64::new(leased),
            current: std::sync::atomic::AtomicU64::new(current),
            entered: None,
            available: true,
        })
    }

    fn unavailable(epoch: u64) -> Arc<Self> {
        Arc::new(Self {
            committed: std::sync::atomic::AtomicU64::new(epoch),
            current: std::sync::atomic::AtomicU64::new(epoch),
            entered: None,
            available: false,
        })
    }

    fn blocked(epoch: u64) -> (Arc<Self>, Arc<tokio::sync::Semaphore>) {
        let entered = Arc::new(tokio::sync::Semaphore::new(0));
        (
            Arc::new(Self {
                committed: std::sync::atomic::AtomicU64::new(epoch),
                current: std::sync::atomic::AtomicU64::new(epoch),
                entered: Some(entered.clone()),
                available: true,
            }),
            entered,
        )
    }

    fn settle(&self) {
        let current = self.current.load(std::sync::atomic::Ordering::SeqCst);
        self.committed.store(current, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl peryx_driver::state::OwnershipAuthority for EpochAuthority {
    async fn committed_epoch(&self, _authority: &str) -> u64 {
        self.committed.load(std::sync::atomic::Ordering::SeqCst)
    }

    async fn admit_epoch(&self, _authority: &str, presented: u64) -> bool {
        if let Some(entered) = &self.entered {
            entered.add_permits(1);
            return std::future::pending().await;
        }
        let current = self.current.load(std::sync::atomic::Ordering::SeqCst);
        current != 0 && presented == current
    }

    async fn begin_epoch_write(
        &self,
        authority: &str,
        presented: u64,
    ) -> Result<Option<peryx_ha::AuthorityWriteLease>, peryx_ha::OwnershipError> {
        if !self.available {
            return Err(peryx_ha::OwnershipError::Unavailable("quorum unavailable".to_owned()));
        }
        if let Some(entered) = &self.entered {
            entered.add_permits(1);
            return std::future::pending().await;
        }
        let current = self.current.load(std::sync::atomic::Ordering::SeqCst);
        Ok(
            (current != 0 && presented == current).then(|| peryx_ha::AuthorityWriteLease {
                authority: authority.to_owned(),
                epoch: presented,
                id: "test-write".to_owned(),
                expires_at_unix: i64::MAX,
            }),
        )
    }

    async fn finish_epoch_write(&self, _lease: &peryx_ha::AuthorityWriteLease) -> Result<(), peryx_ha::OwnershipError> {
        Ok(())
    }

    async fn claim_home(
        &self,
        _authority: &str,
    ) -> Result<peryx_driver::state::HomeClaim, peryx_driver::state::OwnershipError> {
        if !self.available {
            return Err(peryx_ha::OwnershipError::Unavailable("quorum unavailable".to_owned()));
        }
        Ok(peryx_driver::state::HomeClaim {
            home: "local".to_owned(),
            epoch: self.committed.load(std::sync::atomic::Ordering::SeqCst),
        })
    }

    fn cluster_status(&self) -> peryx_driver::state::ClusterStatus {
        peryx_driver::state::ClusterStatus {
            leader: None,
            term: self.current.load(std::sync::atomic::Ordering::SeqCst),
            voters: Vec::new(),
        }
    }

    async fn transfer_home(
        &self,
        _authority: &str,
        _new_home: &str,
    ) -> Result<Option<peryx_driver::state::TransferOutcome>, peryx_driver::state::OwnershipError> {
        Ok(None)
    }
}
