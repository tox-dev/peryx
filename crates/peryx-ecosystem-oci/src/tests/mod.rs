//! Serving tests for the OCI driver, driven end to end through the router with a wiremock upstream.

mod conformance_tests;
mod contents_tests;
mod discovery_tests;
mod metrics_tests;
mod mirror_tests;
mod negotiate_tests;
mod policy_tests;
mod push_tests;
mod search_tests;
mod serve;
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
use peryx_http::router;
use peryx_identity::IndexAcl;
use peryx_index::{Index, IndexKind};
use peryx_policy::Policy;
use peryx_storage::blob::{BlobStore, Digest};
use peryx_storage::meta::MetaStore;
use peryx_upstream::UpstreamClient;
use tempfile::TempDir;
use tower::ServiceExt as _;

use crate::IndexSettings;

/// Build an app over a single OCI index at route `route`, wiring the real driver.
fn app_with(dir: &TempDir, index: Index) -> (Arc<AppState>, axum::Router) {
    app_with_indexes(dir, vec![index])
}

/// Build an app over several indexes, wiring the real driver.
fn app_with_indexes(dir: &TempDir, indexes: Vec<Index>) -> (Arc<AppState>, axum::Router) {
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let mut state = AppState::with_clock(meta, blobs, 60, indexes, Arc::new(|| 1000));
    crate::install(&mut state, HashMap::new());
    let state = Arc::new(state);
    (state.clone(), router(state))
}

/// An OCI index of the given kind, mounted at `route`.
fn oci_index(name: &str, route: &str, kind: IndexKind) -> Index {
    Index {
        name: name.to_owned(),
        route: route.to_owned(),
        ecosystem: Ecosystem::Oci,
        kind,
        policy: Policy::default(),
        acl: IndexAcl::default(),
    }
}

/// A hosted OCI index whose one credential is the upload token `token`.
fn writable_index(name: &str, route: &str, volatile: bool, token: &str) -> Index {
    Index {
        acl: IndexAcl::upload_token(token),
        ..oci_index(name, route, IndexKind::Hosted { volatile })
    }
}

/// A caching proxy of `upstream`, at route `hub`.
fn proxy(dir: &TempDir, upstream: &str, offline: bool) -> (Arc<AppState>, axum::Router) {
    let client = UpstreamClient::new(upstream).unwrap();
    app_with(dir, oci_index("hub", "hub", IndexKind::Cached { client, offline }))
}

/// A caching proxy of `upstream` at route `hub`, under the OCI settings an operator configured for it.
fn proxy_with_settings(dir: &TempDir, upstream: &str, settings: IndexSettings) -> (Arc<AppState>, axum::Router) {
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let client = UpstreamClient::new(upstream).unwrap();
    let index = oci_index("hub", "hub", IndexKind::Cached { client, offline: false });
    let mut state = AppState::with_clock(meta, blobs, 60, vec![index], Arc::new(|| 1000));
    crate::install(&mut state, HashMap::from([("hub".to_owned(), settings)]));
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
    crate::install(&mut state, HashMap::new());
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
    crate::install(&mut state, HashMap::new());
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
            ecosystem: Ecosystem::Pypi,
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

#[tokio::test]
async fn test_v2_reads_are_rate_limited_through_the_oci_classifier() {
    // With the limiter on, the OCI driver classifies a manifest read as a listing, so a second read
    // from the same client is denied. This proves `/v2/` traffic is classed by the registered namespace
    // driver, not the neutral fallback.
    use peryx_driver::rate_limit::{RateLimitConfig, RouteLimit};

    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let rate_limit = RateLimitConfig {
        listing: RouteLimit::new(1, 60),
        ..RateLimitConfig::enabled_defaults()
    };
    let index = oci_index("store", "store", IndexKind::Hosted { volatile: false });
    let mut state = AppState::with_limits(
        meta,
        blobs,
        60,
        vec![index],
        Arc::new(|| 1000),
        rate_limit,
        Vec::<(String, usize)>::new(),
    );
    crate::install(&mut state, HashMap::new());
    let app = router(Arc::new(state));
    let headers = [("x-forwarded-for", "192.0.2.9")];

    let (first, ..) = send_with(&app, Method::GET, "/v2/store/app/manifests/1.0", &headers).await;
    let (second, ..) = send_with(&app, Method::GET, "/v2/store/app/manifests/1.0", &headers).await;

    assert_eq!(first, StatusCode::NOT_FOUND);
    assert_eq!(second, StatusCode::TOO_MANY_REQUESTS);
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
