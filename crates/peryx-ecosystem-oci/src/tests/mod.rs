//! Serving tests for the OCI driver, driven end to end through the router with a wiremock upstream.

mod authority;
mod bearer_tests;
mod conformance_tests;
mod contents_tests;
mod discovery_tests;
mod frontier;
mod metrics_tests;
mod mirror_tests;
mod negotiate_tests;
mod outbox_tests;
mod policy_tests;
mod push_tests;
mod quota_tests;
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
use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
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

/// An ACL whose one named token writes and deletes across every project, for tests that need a
/// hosted index that accepts uploads.
fn writer_acl(secret: impl Into<String>) -> IndexAcl {
    IndexAcl {
        anonymous_read: true,
        tokens: vec![NamedToken {
            name: "uploader".to_owned(),
            secret: secret.into(),
            grants: vec![Grant {
                projects: vec![Glob::new("*")],
                actions: std::collections::BTreeSet::from([Action::Write, Action::Delete]),
            }],
            expires_at: None,
        }],
    }
}

/// Build an app over a single OCI index at route `route`, wiring the real driver.
fn app_with(dir: &TempDir, index: Index) -> (Arc<AppState>, axum::Router) {
    app_with_indexes(dir, vec![index])
}

/// Build an app over several indexes, wiring the real driver.
fn app_with_indexes(dir: &TempDir, indexes: Vec<Index>) -> (Arc<AppState>, axum::Router) {
    app_with_journal(dir, indexes, false)
}

/// Build an app over several indexes, choosing whether authoritative mutations record outbox entries.
fn app_with_journal(dir: &TempDir, indexes: Vec<Index>, journal: bool) -> (Arc<AppState>, axum::Router) {
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let mut state = AppState::with_clock(meta, blobs, 60, indexes, Arc::new(|| 1000));
    crate::install(&mut state, HashMap::new(), journal);
    let state = Arc::new(state);
    (state.clone(), router(state))
}

/// A read-only replica over the same store as `state`, for the derived-view frontier a replica gates
/// hosted reads on. It shares the metadata and blobs, so it serves what the primary published, but its
/// read-only posture is what holds a read behind the readable frontier.
fn replica_router(state: &Arc<AppState>, indexes: Vec<Index>) -> (Arc<AppState>, axum::Router) {
    let mut replica = AppState::with_clock(state.meta.clone(), state.blobs.clone(), 60, indexes, Arc::new(|| 1000));
    crate::install(&mut replica, HashMap::new(), false);
    replica.read_only = true;
    let replica = Arc::new(replica);
    (replica.clone(), router(replica))
}

/// An OCI index of the given kind, mounted at `route`.
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

/// A hosted OCI index whose one credential is the upload token `token`.
fn writable_index(name: &str, route: &str, volatile: bool, token: &str) -> Index {
    Index {
        acl: writer_acl(token),
        ..oci_index(name, route, IndexKind::Hosted { volatile })
    }
}

/// A hosted OCI index with a Bearer token realm signing key installed, so `/v2/` challenges and the
/// `/v2/token` endpoint mint JWTs. The clock reads real time: `jsonwebtoken` validates a token's `exp`
/// against the wall clock, so a token minted here has to expire in the real future to verify.
fn realm_app(dir: &TempDir, indexes: Vec<Index>) -> (Arc<AppState>, axum::Router) {
    realm_app_with_clock_and_limits(dir, indexes, Arc::new(current_unix_time), RateLimitConfig::default())
}

fn realm_app_with_clock_and_limits(
    dir: &TempDir,
    indexes: Vec<Index>,
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    rate_limit: RateLimitConfig,
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
    crate::install(&mut state, HashMap::new(), false);
    state.set_token_realm(Signer::new(b"realm-test-signing-key", crate::TOKEN_SERVICE), 300);
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

/// A hosted OCI index that gates reads (`anonymous_read = false`) behind a single named token whose
/// grant covers `glob` for `actions`.
fn scoped_index(name: &str, route: &str, token: &str, secret: &str, glob: &str, actions: &[Action]) -> Index {
    Index {
        acl: IndexAcl {
            anonymous_read: false,
            tokens: vec![NamedToken {
                name: token.to_owned(),
                secret: secret.to_owned(),
                grants: vec![Grant {
                    projects: vec![Glob::new(glob)],
                    actions: actions.iter().copied().collect(),
                }],
                expires_at: None,
            }],
        },
        ..oci_index(name, route, IndexKind::Hosted { volatile: true })
    }
}

/// The `token` field of a `/v2/token` response body.
fn token_from(body: &Bytes) -> String {
    serde_json::from_slice::<serde_json::Value>(body).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// A caching proxy of `upstream`, at route `hub`.
fn proxy(dir: &TempDir, upstream: &str, offline: bool) -> (Arc<AppState>, axum::Router) {
    let client = UpstreamClient::new(upstream).unwrap();
    app_with(dir, oci_index("hub", "hub", IndexKind::Cached { client, offline }))
}

/// Two caching proxies, `hub` of `up_a` and `vault` of `up_b`, over one pair of stores: the manifest
/// bytes dedup into a single content pool, but each proxy authorizes against its own upstream.
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

/// A caching proxy of `upstream` at route `hub`, under the OCI settings an operator configured for it.
fn proxy_with_settings(dir: &TempDir, upstream: &str, settings: IndexSettings) -> (Arc<AppState>, axum::Router) {
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let client = UpstreamClient::new(upstream).unwrap();
    let index = oci_index("hub", "hub", IndexKind::Cached { client, offline: false });
    let mut state = AppState::with_clock(meta, blobs, 60, vec![index], Arc::new(|| 1000));
    crate::install(&mut state, HashMap::from([("hub".to_owned(), settings)]), false);
    let state = Arc::new(state);
    (state.clone(), router(state))
}

/// A caching proxy of `upstream` at route `hub` that authenticates with `auth` at the token realm.
fn proxy_with_auth(dir: &TempDir, upstream: &str, auth: peryx_upstream::Auth) -> (Arc<AppState>, axum::Router) {
    let client = UpstreamClient::with_auth(upstream, auth).unwrap();
    app_with(
        dir,
        oci_index("hub", "hub", IndexKind::Cached { client, offline: false }),
    )
}

/// A caching proxy of `upstream` at route `hub` reading the current time from `clock`, so a test can
/// advance time past the tag freshness window.
fn proxy_with_clock(
    dir: &TempDir,
    upstream: &str,
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
) -> (Arc<AppState>, axum::Router) {
    proxy_with_stale(dir, upstream, clock, peryx_driver::DEFAULT_MAX_STALE_SECS)
}

/// A caching proxy whose stale-on-error bound the caller chooses; `0` serves stale without limit.
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
    state.max_stale_secs = max_stale_secs;
    crate::install(&mut state, HashMap::new(), false);
    let state = Arc::new(state);
    (state.clone(), router(state))
}

/// A hosted store at route `store`.
fn hosted(dir: &TempDir) -> (Arc<AppState>, axum::Router) {
    app_with(dir, oci_index("store", "store", IndexKind::Hosted { volatile: false }))
}

/// A hosted store at route `store` that accepts uploads bearing `token`.
fn hosted_writable(dir: &TempDir, token: &str) -> (Arc<AppState>, axum::Router) {
    app_with(dir, writable_index("store", "store", true, token))
}

/// A hosted store reading the current time from `clock`, so a test can age out an upload session.
fn hosted_with_clock(
    dir: &TempDir,
    token: &str,
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
) -> (Arc<AppState>, axum::Router) {
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let index = writable_index("store", "store", true, token);
    let mut state = AppState::with_clock(meta, blobs, 60, vec![index], clock);
    crate::install(&mut state, HashMap::new(), false);
    let state = Arc::new(state);
    (state.clone(), router(state))
}

/// A virtual index `reg` stacking a hosted store (`images`, token `s3cret`) over a proxy of
/// `upstream` (`hub`), with uploads routed to the hosted layer.
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
                    upload: Some(0),
                },
            ),
        ],
    )
}

/// A Basic `Authorization` header value carrying `token` as the password (the upload convention).
fn auth(token: &str) -> String {
    use base64::Engine as _;
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("_:{token}"))
    )
}

/// Send a request with a body and extra headers.
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

/// Send a request and collect the status, headers, and body.
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

/// Send a request carrying extra headers.
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

#[tokio::test]
async fn test_v2_without_an_oci_index_is_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = app_with(
        &dir,
        Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: Ecosystem::new("other"),
            kind: IndexKind::Hosted { volatile: false },
            policy: Policy::default(),
            acl: IndexAcl::default(),
        },
    );
    // With no OCI index configured, no driver claims `/v2/`, so the router never mounts it and the
    // request falls to the neutral catch-all.
    let (status, _, body) = send(&app, Method::GET, "/v2/").await;
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

#[tokio::test]
async fn test_unsupported_method_on_a_route_is_declined() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, "http://127.0.0.1:1/", false);
    // PATCH has no meaning on a tags route.
    let (status, _, body) = send(&app, Method::PATCH, "/v2/hub/app/tags/list").await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert!(body_has_code(&body, "UNSUPPORTED"), "{body:?}");
}

#[tokio::test]
async fn test_unknown_route_reports_name_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, "http://127.0.0.1:1/", false);
    let (status, _, body) = send(&app, Method::GET, "/v2/hub/app/frobnicate/x").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
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
    crate::install(&mut state, HashMap::new(), false);
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

/// Whether an error body carries the given distribution-spec code.
fn body_has_code(body: &Bytes, code: &str) -> bool {
    let text = std::str::from_utf8(body).unwrap_or("");
    text.contains(&format!("\"{code}\""))
}

/// The `sha256:<hex>` OCI digest of some bytes.
fn oci_digest(bytes: &[u8]) -> String {
    format!("sha256:{}", Digest::of(bytes).as_str())
}

/// A stand-in ownership group for upload-fence tests. `committed` is the epoch a finalize leases (the
/// local view it reads); `current` is the authoritative epoch the fence admits against. When the two
/// differ, an upload that leased `committed` is fenced, modelling a repository home that transferred
/// while the bytes arrived. `homed` records whether this node holds the home, so a test drives home and
/// non-home ingress; the fence reads only the epoch, so a non-home node at the current epoch finalizes
/// exactly like the home.
struct EpochAuthority {
    committed: std::sync::atomic::AtomicU64,
    current: std::sync::atomic::AtomicU64,
    homed: bool,
}

impl EpochAuthority {
    /// A settled authority whose leased and committed epochs agree, so an upload finalizes.
    fn settled(epoch: u64, homed: bool) -> Arc<Self> {
        Arc::new(Self {
            committed: std::sync::atomic::AtomicU64::new(epoch),
            current: std::sync::atomic::AtomicU64::new(epoch),
            homed,
        })
    }

    /// An authority that advanced past the epoch a finalize leased, so the upload is fenced.
    fn superseded(leased: u64, current: u64) -> Arc<Self> {
        Arc::new(Self {
            committed: std::sync::atomic::AtomicU64::new(leased),
            current: std::sync::atomic::AtomicU64::new(current),
            homed: true,
        })
    }

    /// Catch the local view up to the current epoch, so a retry after the transfer settles finalizes.
    fn settle(&self) {
        let current = self.current.load(std::sync::atomic::Ordering::SeqCst);
        self.committed.store(current, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl peryx_driver::state::OwnershipAuthority for EpochAuthority {
    async fn has_home(&self, _authority: &str) -> bool {
        self.homed
    }

    async fn committed_epoch(&self, _authority: &str) -> u64 {
        self.committed.load(std::sync::atomic::Ordering::SeqCst)
    }

    async fn admit_epoch(&self, _authority: &str, presented: u64) -> bool {
        let current = self.current.load(std::sync::atomic::Ordering::SeqCst);
        current != 0 && presented == current
    }

    async fn claim_home(
        &self,
        _authority: &str,
    ) -> Result<peryx_driver::state::HomeClaim, peryx_driver::state::OwnershipError> {
        Ok(peryx_driver::state::HomeClaim::AlreadyHomed)
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
        // Upload-fence tests never move a home; the fence reads only the committed and current epochs.
        Ok(None)
    }
}
