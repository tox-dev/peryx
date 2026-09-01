//! Every response leaves the process with the browser defences its handler did not set for itself.

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Method, Request, StatusCode, Uri, header};
use axum::response::{IntoResponse as _, Redirect, Response};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use peryx_core::Ecosystem;
use peryx_driver::rate_limit::{RateLimitConfig, RouteClass, RouteLimit};
use peryx_driver::serving::{EcosystemDriver, IndexedProtocolDriver, ProtocolDriver};
use peryx_driver::state::{AppState, Index, IndexKind, ServingState};
use peryx_identity::{Action, Glob, Grant, IndexAcl, NamedToken};
use rstest::rstest;
use tower::ServiceExt as _;

const WRITER_SECRET: &str = "writer-secret";

struct StubDriver;

impl EcosystemDriver for StubDriver {
    fn ecosystem(&self) -> Ecosystem {
        Ecosystem::new("example")
    }
}

#[async_trait::async_trait]
impl IndexedProtocolDriver for StubDriver {
    fn classify_route(&self, _path: &str) -> RouteClass {
        RouteClass::Artifact
    }

    async fn get(
        &self,
        _state: Arc<ServingState>,
        _position: usize,
        rest: String,
        _uri: Uri,
        _headers: HeaderMap,
        _method: Method,
    ) -> Response {
        match rest.as_str() {
            "page" => ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], "<p>peryx</p>").into_response(),
            "page-without-parameters" => ([(header::CONTENT_TYPE, "text/html")], "<p>peryx</p>").into_response(),
            "shouty-page" => ([(header::CONTENT_TYPE, "TEXT/HTML")], "<p>peryx</p>").into_response(),
            "look-alike" => ([(header::CONTENT_TYPE, "text/html-ish")], "<p>peryx</p>").into_response(),
            "export" => ([(header::CONTENT_TYPE, "text/csv")], "name,version\n").into_response(),
            "artifact" => (
                [(header::CONTENT_TYPE, "application/octet-stream")],
                vec![0_u8, 1, 2, 3],
            )
                .into_response(),
            "unchanged" => (
                StatusCode::NOT_MODIFIED,
                [(header::CACHE_CONTROL, "public, max-age=60")],
            )
                .into_response(),
            "elsewhere" => Redirect::temporary("/alpha/page").into_response(),
            "opinionated" => (
                [
                    (header::CONTENT_TYPE, "text/html"),
                    (
                        header::CONTENT_SECURITY_POLICY,
                        "default-src 'none'; frame-ancestors 'none'",
                    ),
                    (header::X_FRAME_OPTIONS, "SAMEORIGIN"),
                    (header::REFERRER_POLICY, "same-origin"),
                    (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                    (header::STRICT_TRANSPORT_SECURITY, "max-age=63072000"),
                ],
                "<p>peryx</p>",
            )
                .into_response(),
            _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    }

    async fn post(&self, _state: Arc<ServingState>, _path: String, _request: Request<Body>) -> Response {
        StatusCode::NO_CONTENT.into_response()
    }

    async fn put(&self, _state: Arc<ServingState>, _request: Request<Body>) -> Response {
        StatusCode::NO_CONTENT.into_response()
    }

    async fn delete(&self, _state: Arc<ServingState>, _request: axum::extract::Request) -> Response {
        StatusCode::NO_CONTENT.into_response()
    }
}

fn state(rate_limit: RateLimitConfig) -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let meta = peryx_storage::meta::MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = peryx_storage::blob::BlobStore::new(dir.path().join("blobs"));
    let index = Index {
        name: "alpha".to_owned(),
        route: "alpha".to_owned(),
        ecosystem: Ecosystem::new("example"),
        kind: IndexKind::Hosted { volatile: false },
        policy: peryx_policy::Policy::default(),
        acl: IndexAcl {
            anonymous_read: true,
            tokens: vec![NamedToken {
                name: "writer".to_owned(),
                secret: WRITER_SECRET.to_owned(),
                grants: vec![Grant {
                    resources: vec![Glob::new("*")],
                    actions: BTreeSet::from([Action::Write, Action::Delete]),
                }],
                expires_at: None,
            }],
        },
    };
    let mut state = AppState::with_rate_limits(meta, blobs, 60, vec![index], rate_limit, []);
    state
        .register_protocol(
            ProtocolDriver::Indexed(Arc::new(StubDriver)),
            peryx_search::default_indexer(),
        )
        .unwrap();
    (dir, state)
}

async fn respond(state: AppState, request: Request<Body>) -> Response {
    crate::router(Arc::new(state)).oneshot(request).await.unwrap()
}

async fn fetch(state: AppState, uri: &str) -> Response {
    respond(state, Request::builder().uri(uri).body(Body::empty()).unwrap()).await
}

fn value(response: &Response, name: header::HeaderName) -> Option<&str> {
    response.headers().get(name).map(|value| value.to_str().unwrap())
}

#[rstest]
#[case::html("/alpha/page")]
#[case::artifact("/alpha/artifact")]
#[case::error("/alpha/anything-else")]
#[case::redirect("/alpha/elsewhere")]
#[case::not_modified("/alpha/unchanged")]
#[case::framework_not_found("/")]
#[tokio::test]
async fn test_every_response_refuses_content_sniffing(#[case] uri: &str) {
    let (_dir, state) = state(RateLimitConfig::default());

    let response = fetch(state, uri).await;

    assert_eq!(value(&response, header::X_CONTENT_TYPE_OPTIONS), Some("nosniff"));
}

#[tokio::test]
async fn test_a_method_the_route_does_not_accept_refuses_content_sniffing() {
    let (_dir, state) = state(RateLimitConfig::default());

    let response = respond(
        state,
        Request::builder()
            .method(Method::PATCH)
            .uri("/+status")
            .body(Body::empty())
            .unwrap(),
    )
    .await;

    assert_eq!(
        (response.status(), value(&response, header::X_CONTENT_TYPE_OPTIONS)),
        (StatusCode::METHOD_NOT_ALLOWED, Some("nosniff"))
    );
}

#[rstest]
#[case::with_parameters("/alpha/page")]
#[case::without_parameters("/alpha/page-without-parameters")]
#[case::uppercase_media_type("/alpha/shouty-page")]
#[tokio::test]
async fn test_a_rendered_document_rejects_framing_object_content_and_referrers(#[case] uri: &str) {
    let (_dir, state) = state(RateLimitConfig::default());

    let response = fetch(state, uri).await;

    assert_eq!(
        (
            value(&response, header::CONTENT_SECURITY_POLICY),
            value(&response, header::X_FRAME_OPTIONS),
            value(&response, header::REFERRER_POLICY),
        ),
        (
            Some("frame-ancestors 'none'; base-uri 'none'; object-src 'none'"),
            Some("DENY"),
            Some("no-referrer"),
        )
    );
}

#[rstest]
#[case::artifact("/alpha/artifact")]
#[case::redirect("/alpha/elsewhere")]
#[case::media_type_that_only_starts_like_html("/alpha/look-alike")]
#[case::media_type_shorter_than_html("/alpha/export")]
#[tokio::test]
async fn test_a_response_that_is_not_a_document_carries_no_document_policy(#[case] uri: &str) {
    let (_dir, state) = state(RateLimitConfig::default());

    let response = fetch(state, uri).await;

    assert_eq!(
        (
            value(&response, header::CONTENT_SECURITY_POLICY),
            value(&response, header::X_FRAME_OPTIONS),
            value(&response, header::REFERRER_POLICY),
        ),
        (None, None, None)
    );
}

#[tokio::test]
async fn test_a_handlers_own_security_headers_survive_the_defaults() {
    let (_dir, mut state) = state(RateLimitConfig::default());
    state.set_tls_terminated(true).unwrap();

    let response = fetch(state, "/alpha/opinionated").await;

    assert_eq!(
        (
            value(&response, header::CONTENT_SECURITY_POLICY),
            value(&response, header::X_FRAME_OPTIONS),
            value(&response, header::REFERRER_POLICY),
            value(&response, header::STRICT_TRANSPORT_SECURITY),
        ),
        (
            Some("default-src 'none'; frame-ancestors 'none'"),
            Some("SAMEORIGIN"),
            Some("same-origin"),
            Some("max-age=63072000"),
        )
    );
}

#[tokio::test]
async fn test_a_not_modified_response_keeps_its_cache_policy() {
    let (_dir, state) = state(RateLimitConfig::default());

    let response = fetch(state, "/alpha/unchanged").await;

    assert_eq!(
        (response.status(), value(&response, header::CACHE_CONTROL)),
        (StatusCode::NOT_MODIFIED, Some("public, max-age=60"))
    );
}

#[tokio::test]
async fn test_terminating_tls_here_pins_the_transport() {
    let (_dir, mut state) = state(RateLimitConfig::default());
    state.set_tls_terminated(true).unwrap();

    let response = fetch(state, "/alpha/artifact").await;

    assert_eq!(
        value(&response, header::STRICT_TRANSPORT_SECURITY),
        Some("max-age=31536000")
    );
}

#[rstest]
#[case::no_peer(None, "https", None)]
#[case::untrusted_peer(Some("192.0.2.1:443"), "https", None)]
#[case::trusted_peer_over_cleartext(Some("127.0.0.1:443"), "http", None)]
#[case::trusted_peer_over_tls(Some("127.0.0.1:443"), "https", Some("max-age=31536000"))]
#[case::trusted_peer_behind_a_chain(Some("127.0.0.1:443"), "https, http", Some("max-age=31536000"))]
#[tokio::test]
async fn test_a_forwarded_scheme_pins_the_transport_only_from_a_trusted_proxy(
    #[case] peer: Option<&str>,
    #[case] forwarded: &str,
    #[case] expected: Option<&str>,
) {
    let (_dir, state) = state(RateLimitConfig {
        trusted_proxies: vec!["127.0.0.1/32".parse().unwrap()],
        ..RateLimitConfig::default()
    });
    let mut request = Request::builder()
        .uri("/alpha/artifact")
        .header("x-forwarded-proto", forwarded)
        .body(Body::empty())
        .unwrap();
    if let Some(peer) = peer {
        request
            .extensions_mut()
            .insert(ConnectInfo(peer.parse::<std::net::SocketAddr>().unwrap()));
    }

    let response = respond(state, request).await;

    assert_eq!(value(&response, header::STRICT_TRANSPORT_SECURITY), expected);
}

#[rstest]
#[case::post(Method::POST)]
#[case::put(Method::PUT)]
#[case::delete(Method::DELETE)]
#[tokio::test]
async fn test_a_mutation_response_refuses_content_sniffing(#[case] method: Method) {
    let (_dir, state) = state(RateLimitConfig::default());
    let credential = STANDARD.encode(format!("anyuser:{WRITER_SECRET}"));

    let response = respond(
        state,
        Request::builder()
            .method(method)
            .uri("/alpha/artifact")
            .header(header::AUTHORIZATION, format!("Basic {credential}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;

    assert_eq!(
        (response.status(), value(&response, header::X_CONTENT_TYPE_OPTIONS)),
        (StatusCode::NO_CONTENT, Some("nosniff"))
    );
}

#[tokio::test]
async fn test_a_rate_limited_rejection_refuses_content_sniffing() {
    let (_dir, state) = state(RateLimitConfig {
        artifact: RouteLimit::new(1, 60),
        ..RateLimitConfig::enabled_defaults()
    });
    let router = crate::router(Arc::new(state));
    let spent = router
        .clone()
        .oneshot(Request::builder().uri("/alpha/artifact").body(Body::empty()).unwrap())
        .await
        .unwrap();

    let response = router
        .oneshot(Request::builder().uri("/alpha/artifact").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(
        (
            spent.status(),
            response.status(),
            value(&response, header::X_CONTENT_TYPE_OPTIONS)
        ),
        (StatusCode::OK, StatusCode::TOO_MANY_REQUESTS, Some("nosniff"))
    );
}

#[tokio::test]
async fn test_a_trusted_proxy_that_forwards_no_scheme_leaves_the_transport_unpinned() {
    let (_dir, state) = state(RateLimitConfig {
        trusted_proxies: vec!["127.0.0.1/32".parse().unwrap()],
        ..RateLimitConfig::default()
    });
    let mut request = Request::builder().uri("/alpha/artifact").body(Body::empty()).unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo("127.0.0.1:443".parse::<std::net::SocketAddr>().unwrap()));

    let response = respond(state, request).await;

    assert_eq!(value(&response, header::STRICT_TRANSPORT_SECURITY), None);
}

#[tokio::test]
async fn test_cleartext_service_leaves_the_transport_unpinned() {
    let (_dir, state) = state(RateLimitConfig::default());

    let response = fetch(state, "/alpha/artifact").await;

    assert_eq!(value(&response, header::STRICT_TRANSPORT_SECURITY), None);
}
