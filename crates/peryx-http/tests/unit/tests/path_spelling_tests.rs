//! One request path, one spelling: the dispatchers, the read-only guard and the rate limiter must
//! all resolve a request from the same characters.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, Uri, header};
use axum::response::{IntoResponse as _, Response};
use peryx_core::Ecosystem;
use peryx_driver::rate_limit::{RateLimitConfig, RouteClass, RouteLimit};
use peryx_driver::serving::{
    EcosystemDriver, IndexedProtocolDriver, ProtocolDriver, RateLimitPrincipal, ServiceDriver,
};
use peryx_driver::state::{AppState, Index, IndexKind, ServingState};
use peryx_identity::{IndexAcl, Principal};
use rstest::rstest;
use tower::ServiceExt as _;

struct StubDriver;

impl EcosystemDriver for StubDriver {
    fn ecosystem(&self) -> Ecosystem {
        Ecosystem::new("example")
    }
}

impl RateLimitPrincipal for StubDriver {
    fn resolve(&self, _state: &ServingState, _position: Option<usize>, _headers: &HeaderMap) -> Principal {
        Principal::Named {
            subject: "publisher".to_owned(),
        }
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
        _rest: String,
        uri: Uri,
        _headers: HeaderMap,
        _method: Method,
    ) -> Response {
        uri.to_string().into_response()
    }

    async fn post(&self, _state: Arc<ServingState>, path: String, _request: Request<Body>) -> Response {
        path.into_response()
    }

    async fn put(&self, _state: Arc<ServingState>, _request: Request<Body>) -> Response {
        StatusCode::NO_CONTENT.into_response()
    }

    async fn delete(&self, _state: Arc<ServingState>, _uri: Uri, _headers: HeaderMap) -> Response {
        StatusCode::NO_CONTENT.into_response()
    }
}

#[async_trait::async_trait]
impl ServiceDriver for StubDriver {
    fn classify_service_post(&self, path: &str, _headers: &HeaderMap) -> Option<RouteClass> {
        (path == "service/read").then_some(RouteClass::Listing)
    }

    async fn service_post(&self, _state: Arc<ServingState>, _request: Request<Body>) -> Response {
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
        acl: IndexAcl::default(),
    };
    let mut state = AppState::with_rate_limits(meta, blobs, 60, vec![index], rate_limit, []);
    state.register_rate_limit_principal(Ecosystem::new("example"), &StubDriver);
    state.register_capabilities(|registrar| {
        registrar.register_service(Ecosystem::new("example"), Arc::new(StubDriver));
    });
    state
        .register_protocol(
            ProtocolDriver::Indexed(Arc::new(StubDriver)),
            peryx_search::default_indexer(),
        )
        .unwrap();
    (dir, state)
}

#[rstest]
#[case::plain("/service/read")]
#[case::encoded("/%73ervice/%72ead")]
#[tokio::test]
async fn test_a_service_read_post_is_a_read_on_a_replica_whatever_its_spelling(#[case] uri: &str) {
    let (_dir, mut state) = state(RateLimitConfig::default());
    state.set_read_only(true).unwrap();

    let response = crate::router(Arc::new(state))
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

#[rstest]
#[case::plain("/service/read")]
#[case::encoded("/%73ervice/%72ead")]
#[tokio::test]
async fn test_a_service_read_post_is_a_read_on_a_writer_whatever_its_spelling(#[case] uri: &str) {
    let (_dir, state) = state(RateLimitConfig::default());

    let response = crate::router(Arc::new(state))
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn test_an_encoded_upload_shares_the_plain_spellings_principal_bucket() {
    let (_dir, state) = state(RateLimitConfig {
        upload: RouteLimit::new(1, 60),
        ..RateLimitConfig::enabled_defaults()
    });
    let router = crate::router(Arc::new(state));
    let upload = |uri: &'static str| {
        Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header(header::AUTHORIZATION, "Bearer token")
            .body(Body::empty())
            .unwrap()
    };

    let first = router.clone().oneshot(upload("/alpha/")).await.unwrap();
    let second = router.oneshot(upload("/%61lpha/")).await.unwrap();

    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn test_an_upload_reaches_the_driver_with_the_path_the_limiter_read() {
    let (_dir, state) = state(RateLimitConfig::default());

    let response = crate::router(Arc::new(state))
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/%61lpha/")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap(),
        "alpha/"
    );
}

#[tokio::test]
async fn test_an_encoded_separator_names_a_segment_rather_than_an_index_route() {
    let (_dir, state) = state(RateLimitConfig::default());

    let response = crate::router(Arc::new(state))
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/alpha%2F")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[rstest]
#[case::put(Method::PUT)]
#[case::delete(Method::DELETE)]
#[tokio::test]
async fn test_every_dispatcher_resolves_an_index_from_the_canonical_spelling(#[case] method: Method) {
    let (_dir, state) = state(RateLimitConfig::default());

    let response = crate::router(Arc::new(state))
        .oneshot(
            Request::builder()
                .method(method)
                .uri("/%61lpha/item")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn test_canonicalizing_a_path_leaves_the_query_string_alone() {
    let (_dir, state) = state(RateLimitConfig::enabled_defaults());

    let response = crate::router(Arc::new(state))
        .oneshot(
            Request::builder()
                .uri("/%61lpha/resource?flag=1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap(),
        "/alpha/resource?flag=1"
    );
}
