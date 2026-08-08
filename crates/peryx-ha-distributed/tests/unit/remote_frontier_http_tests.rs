use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};

use crate::peer::TransportError;
use crate::remote_frontier::RemoteFrontierSource;
use crate::remote_frontier_http::{
    FrontierReply, HttpRemoteFrontierError, HttpRemoteFrontierSource, MetadataFrontierProvider, frontier_router,
};

const ROUTE: &str = "/+replication/v1/frontier/{authority}";
const TOKEN: &str = "secret";
const AUTHORITY: &str = "proj";

fn source(url: &str, datacenter: &str) -> HttpRemoteFrontierSource {
    HttpRemoteFrontierSource::new(url, datacenter, TOKEN, Duration::from_secs(5)).unwrap()
}

struct TestServer {
    url: String,
    task: tokio::task::JoinHandle<()>,
}

impl TestServer {
    async fn start(router: Router) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            url: format!("http://{address}/"),
            task,
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A frontier endpoint that answers every authority with one preset response, for driving the client's
/// status handling without a provider.
fn serving(response: impl Fn() -> Response + Clone + Send + Sync + 'static) -> Router {
    Router::new().route(
        ROUTE,
        get(move || {
            let response = response.clone();
            async move { response() }
        }),
    )
}

/// A provider returning one fixed answer, for the server round-trip tests.
struct FixedProvider(Option<FrontierReply>);

#[async_trait]
impl MetadataFrontierProvider for FixedProvider {
    async fn frontier(&self, _authority: &str) -> Option<FrontierReply> {
        self.0
    }
}

fn router_over(reply: Option<FrontierReply>) -> Router {
    frontier_router(
        TOKEN,
        Arc::new(FixedProvider(reply)) as Arc<dyn MetadataFrontierProvider>,
    )
    .unwrap()
}

#[test]
fn test_new_rejects_an_empty_token() {
    let error = HttpRemoteFrontierSource::new("http://peer/", "east", "", Duration::from_secs(1)).unwrap_err();

    assert!(matches!(error, HttpRemoteFrontierError::EmptyToken));
}

#[test]
fn test_new_rejects_an_empty_datacenter() {
    let error = HttpRemoteFrontierSource::new("http://peer/", "", TOKEN, Duration::from_secs(1)).unwrap_err();

    assert!(matches!(error, HttpRemoteFrontierError::EmptyDatacenter));
}

#[test]
fn test_new_rejects_an_unparseable_url() {
    let error = HttpRemoteFrontierSource::new("not a url", "east", TOKEN, Duration::from_secs(1)).unwrap_err();

    assert!(matches!(error, HttpRemoteFrontierError::InvalidBase(_)));
}

#[test]
fn test_new_rejects_a_non_http_scheme() {
    let error = HttpRemoteFrontierSource::new("ftp://peer/", "east", TOKEN, Duration::from_secs(1)).unwrap_err();

    assert!(matches!(error, HttpRemoteFrontierError::InvalidBase(_)));
}

#[test]
fn test_debug_redacts_the_token() {
    let rendered = format!("{:?}", source("http://peer.example/", "east"));

    assert!(rendered.contains("<redacted>"), "{rendered}");
    assert!(rendered.contains("east"), "the datacenter is not a secret: {rendered}");
    assert!(!rendered.contains(TOKEN), "token leaked: {rendered}");
}

#[test]
fn test_datacenter_reports_the_configured_remote() {
    assert_eq!(source("http://peer.example/", "west-2").datacenter(), "west-2");
}

#[tokio::test]
async fn test_fetch_parses_a_frontier_from_a_nested_base() {
    let router = Router::new().nest(
        "/mirror",
        serving(|| {
            Json(FrontierReply {
                epoch: 3,
                applied_frontier: 120,
            })
            .into_response()
        }),
    );
    let server = TestServer::start(router).await;
    let source = source(&format!("{}mirror", server.url), "east");

    let ack = source.fetch_frontier(AUTHORITY).await.unwrap().unwrap();

    assert_eq!(ack.datacenter, "east");
    assert_eq!(ack.epoch, 3);
    assert_eq!(ack.applied_frontier, 120);
}

#[tokio::test]
async fn test_fetch_maps_a_non_reporting_remote_to_none() {
    let server = TestServer::start(serving(|| StatusCode::NOT_FOUND.into_response())).await;

    let ack = source(&server.url, "east").fetch_frontier(AUTHORITY).await.unwrap();

    assert_eq!(ack, None);
}

#[tokio::test]
async fn test_fetch_maps_unauthorized_to_unauthenticated() {
    let server = TestServer::start(serving(|| StatusCode::UNAUTHORIZED.into_response())).await;

    let error = source(&server.url, "east").fetch_frontier(AUTHORITY).await.unwrap_err();

    assert_eq!(error, TransportError::Unauthenticated);
}

#[tokio::test]
async fn test_fetch_maps_a_transient_server_error_to_retryable() {
    let server = TestServer::start(serving(|| StatusCode::BAD_GATEWAY.into_response())).await;

    let error = source(&server.url, "east").fetch_frontier(AUTHORITY).await.unwrap_err();

    assert_eq!(error, TransportError::ServerError { status: 502 });
    assert!(error.is_retryable());
}

#[tokio::test]
async fn test_fetch_keeps_not_implemented_terminal() {
    let server = TestServer::start(serving(|| StatusCode::NOT_IMPLEMENTED.into_response())).await;

    let error = source(&server.url, "east").fetch_frontier(AUTHORITY).await.unwrap_err();

    assert_eq!(error, TransportError::BadStatus { status: 501 });
}

#[tokio::test]
async fn test_fetch_maps_an_unexpected_status_to_bad_status() {
    let server = TestServer::start(serving(|| StatusCode::IM_A_TEAPOT.into_response())).await;

    let error = source(&server.url, "east").fetch_frontier(AUTHORITY).await.unwrap_err();

    assert_eq!(error, TransportError::BadStatus { status: 418 });
}

#[tokio::test]
async fn test_fetch_rejects_a_malformed_reply() {
    let server = TestServer::start(serving(|| (StatusCode::OK, "not json").into_response())).await;

    let error = source(&server.url, "east").fetch_frontier(AUTHORITY).await.unwrap_err();

    assert_eq!(error, TransportError::Malformed);
}

#[tokio::test]
async fn test_fetch_rejects_an_oversized_reply() {
    let server = TestServer::start(serving(|| {
        let body = "x".repeat(8192);
        let stream =
            futures_util::stream::once(async move { Ok::<_, std::convert::Infallible>(bytes::Bytes::from(body)) });
        (StatusCode::OK, Body::from_stream(stream)).into_response()
    }))
    .await;

    let error = source(&server.url, "east").fetch_frontier(AUTHORITY).await.unwrap_err();

    assert_eq!(error, TransportError::Malformed);
}

#[tokio::test]
async fn test_fetch_maps_a_dead_remote_to_disconnected() {
    let error = source("http://127.0.0.1:1/", "east")
        .fetch_frontier(AUTHORITY)
        .await
        .unwrap_err();

    assert_eq!(error, TransportError::Disconnected);
}

#[tokio::test]
async fn test_fetch_maps_a_deadline_to_timeout() {
    let server = TestServer::start(serving(|| {
        let stream = futures_util::stream::pending::<Result<bytes::Bytes, std::convert::Infallible>>();
        Body::from_stream(stream).into_response()
    }))
    .await;
    let slow = HttpRemoteFrontierSource::new(&server.url, "east", TOKEN, Duration::from_millis(150)).unwrap();

    let error = slow.fetch_frontier(AUTHORITY).await.unwrap_err();

    assert_eq!(error, TransportError::Timeout);
}

#[test]
fn test_router_rejects_an_empty_token() {
    let error = frontier_router("", Arc::new(FixedProvider(None)) as Arc<dyn MetadataFrontierProvider>).unwrap_err();

    assert!(matches!(error, HttpRemoteFrontierError::EmptyToken));
}

#[tokio::test]
async fn test_endpoint_serves_a_frontier_end_to_end() {
    let server = TestServer::start(router_over(Some(FrontierReply {
        epoch: 5,
        applied_frontier: 200,
    })))
    .await;

    let ack = source(&server.url, "east")
        .fetch_frontier(AUTHORITY)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(ack.epoch, 5);
    assert_eq!(ack.applied_frontier, 200);
}

#[tokio::test]
async fn test_endpoint_reports_a_node_that_cannot_report_as_absent() {
    let server = TestServer::start(router_over(None)).await;

    let ack = source(&server.url, "east").fetch_frontier(AUTHORITY).await.unwrap();

    assert_eq!(ack, None);
}

#[tokio::test]
async fn test_endpoint_rejects_a_bad_credential() {
    let server = TestServer::start(router_over(Some(FrontierReply {
        epoch: 5,
        applied_frontier: 200,
    })))
    .await;
    let wrong = HttpRemoteFrontierSource::new(&server.url, "east", "wrong", Duration::from_secs(5)).unwrap();

    let error = wrong.fetch_frontier(AUTHORITY).await.unwrap_err();

    assert_eq!(error, TransportError::Unauthenticated);
}
