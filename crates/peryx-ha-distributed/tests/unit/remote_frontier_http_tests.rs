use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Json, Router};

use crate::peer::TransportError;
use crate::remote_frontier::RemoteFrontierSource;
use crate::remote_frontier_http::{
    FrontierReadError, FrontierReply, HttpRemoteFrontierError, HttpRemoteFrontierSource, MetadataFrontierProvider,
    frontier_router,
};
use crate::support::{TestServer, http_contract};

const ROUTE: &str = "/+replication/v1/frontier/{authority}";
const TOKEN: &str = "secret";
const AUTHORITY: &str = "proj";

fn source(url: &str, datacenter: &str) -> HttpRemoteFrontierSource {
    HttpRemoteFrontierSource::new(url, datacenter, TOKEN, Duration::from_secs(5)).unwrap()
}

struct FixedProvider(Result<Option<FrontierReply>, FrontierReadError>);

#[async_trait]
impl MetadataFrontierProvider for FixedProvider {
    async fn frontier(&self, _authority: &str) -> Result<Option<FrontierReply>, FrontierReadError> {
        self.0
    }
}

fn router_over(reply: Option<FrontierReply>) -> Router {
    router_answering(Ok(reply))
}

fn router_answering(answer: Result<Option<FrontierReply>, FrontierReadError>) -> Router {
    frontier_router(
        TOKEN,
        Arc::new(FixedProvider(answer)) as Arc<dyn MetadataFrontierProvider>,
    )
    .unwrap()
}

#[test]
fn test_configuration_contract() {
    http_contract::assert_configuration(
        |base, token| HttpRemoteFrontierSource::new(base, "east", token, Duration::from_secs(1)).map(|_| ()),
        |error| matches!(error, HttpRemoteFrontierError::EmptyToken),
        |error| matches!(error, HttpRemoteFrontierError::InvalidBase(_)),
    );
}

#[test]
fn test_new_rejects_an_empty_datacenter() {
    let error = HttpRemoteFrontierSource::new("http://peer/", "", TOKEN, Duration::from_secs(1)).unwrap_err();

    assert!(matches!(error, HttpRemoteFrontierError::EmptyDatacenter));
}

#[test]
fn test_datacenter_reports_the_configured_remote() {
    assert_eq!(source("http://peer.example/", "west-2").datacenter(), "west-2");
}

#[test]
fn test_debug_names_the_datacenter_without_the_token() {
    http_contract::assert_redacted(
        &source("http://peer.example/root", "west-2"),
        TOKEN,
        &["HttpRemoteFrontierSource", "west-2"],
    );
}

#[tokio::test]
async fn test_fetch_parses_a_frontier_from_a_nested_base() {
    let ack = http_contract::run_nested(
        http_contract::fixed_get(ROUTE, || {
            Json(FrontierReply {
                epoch: 3,
                applied_frontier: 120,
            })
            .into_response()
        }),
        |base| async move { source(&base, "east").fetch_frontier(AUTHORITY).await.unwrap().unwrap() },
    )
    .await;

    assert_eq!(ack.datacenter, "east");
    assert_eq!(ack.epoch, 3);
    assert_eq!(ack.applied_frontier, 120);
}

#[tokio::test]
async fn test_fetch_maps_a_non_reporting_remote_to_none() {
    http_contract::assert_mapping(
        http_contract::fixed_get(ROUTE, || StatusCode::NOT_FOUND.into_response()),
        |base| async move { source(&base, "east").fetch_frontier(AUTHORITY).await.unwrap() },
        None,
    )
    .await;
}

#[tokio::test]
async fn test_fetch_rejects_a_malformed_reply() {
    http_contract::assert_mapping(
        http_contract::fixed_get(ROUTE, || (StatusCode::OK, "not json").into_response()),
        |base| async move { source(&base, "east").fetch_frontier(AUTHORITY).await },
        Err(TransportError::Malformed),
    )
    .await;
}

#[test]
fn test_router_rejects_an_empty_token() {
    let error = frontier_router(
        "",
        Arc::new(FixedProvider(Ok(None))) as Arc<dyn MetadataFrontierProvider>,
    )
    .unwrap_err();

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
async fn test_endpoint_reports_a_node_holding_no_position_as_absent() {
    let server = TestServer::start(router_over(None)).await;

    let ack = source(&server.url, "east").fetch_frontier(AUTHORITY).await.unwrap();

    assert_eq!(ack, None);
}

#[tokio::test]
async fn test_endpoint_reports_an_unreadable_position_as_a_retryable_fault() {
    let server = TestServer::start(router_answering(Err(FrontierReadError))).await;

    let error = source(&server.url, "east").fetch_frontier(AUTHORITY).await.unwrap_err();

    assert_eq!(error, TransportError::ServerError { status: 500 });
    assert!(
        error.is_retryable(),
        "a node that cannot read its own position is broken, not empty"
    );
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
