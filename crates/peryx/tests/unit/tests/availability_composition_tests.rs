//! The availability surface joins the composed router at the one seam that carries the browser
//! defaults while staying outside request accounting.

use std::io::{Read as _, Seek as _};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::IntoResponse as _;
use peryx_driver::rate_limit::{RateLimitConfig, RouteLimit};
use rstest::rstest;
use tower::ServiceExt as _;

use crate::config::{AvailabilityConfig, Config, ReplicationConfig, SecretSource};
use crate::replication::ReplicationRuntime;
use crate::server::{build_router, build_state, router_for};

const HEALTH: &str = "/+replication/v1/health";
const READINESS: &str = "/+replication/v1/ready";

fn primary(dir: &tempfile::TempDir, rate_limit: RateLimitConfig) -> Config {
    Config {
        data_dir: dir.path().to_path_buf(),
        availability: AvailabilityConfig::Dc(ReplicationConfig::Primary {
            source: "writer-a".to_owned(),
            token: SecretSource::Literal("replication-token".to_owned()),
        }),
        rate_limit,
        ..Config::default()
    }
}

async fn respond(router: &axum::Router, uri: &str) -> axum::response::Response {
    router
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap()
}

/// Reading the whole policy rather than one header keeps the assertion honest: a route that answers
/// with a subset fails instead of passing on whichever header the probe happened to name.
fn policy(response: &axum::response::Response) -> Vec<(String, String)> {
    [
        header::X_CONTENT_TYPE_OPTIONS,
        header::CONTENT_SECURITY_POLICY,
        header::X_FRAME_OPTIONS,
        header::REFERRER_POLICY,
        header::STRICT_TRANSPORT_SECURITY,
    ]
    .into_iter()
    .filter_map(|name| {
        let value = response.headers().get(&name)?;
        Some((name.to_string(), value.to_str().unwrap().to_owned()))
    })
    .collect()
}

#[rstest]
#[case::health(HEALTH)]
#[case::readiness(READINESS)]
#[tokio::test]
async fn test_an_availability_route_answers_with_the_browser_defaults(#[case] uri: &str) {
    let dir = tempfile::tempdir().unwrap();
    let router = build_router(&primary(&dir, RateLimitConfig::default())).unwrap();

    let response = respond(&router, uri).await;

    assert_eq!(
        (response.status(), policy(&response)),
        (
            StatusCode::OK,
            vec![("x-content-type-options".to_owned(), "nosniff".to_owned())]
        )
    );
}

/// A JSON service route settles what the policy is for a JSON body, so the availability routes
/// answering with the same set is what proves they sit inside the same layer.
#[tokio::test]
async fn test_a_service_route_answers_with_the_same_policy() {
    let dir = tempfile::tempdir().unwrap();
    let router = build_router(&primary(&dir, RateLimitConfig::default())).unwrap();

    let service = respond(&router, "/+health").await;
    let availability = respond(&router, HEALTH).await;

    assert_eq!(policy(&availability), policy(&service));
}

/// A peer polls liveness while a client is spending its budget, so the availability routes join
/// outside request accounting - the seam the static assets already use.
#[rstest]
#[case::health(HEALTH)]
#[case::readiness(READINESS)]
#[tokio::test]
async fn test_an_availability_route_stays_outside_every_rate_limit_budget(#[case] uri: &str) {
    let dir = tempfile::tempdir().unwrap();
    let router = build_router(&primary(
        &dir,
        RateLimitConfig {
            admin: RouteLimit::new(1, 60),
            ..RateLimitConfig::enabled_defaults()
        },
    ))
    .unwrap();
    let spent = respond(&router, "/+status").await.status();
    let rejected = respond(&router, "/+status").await.status();

    let first = respond(&router, uri).await.status();
    let second = respond(&router, uri).await.status();

    assert_eq!(
        (spent, rejected, first, second),
        (
            StatusCode::OK,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::OK,
            StatusCode::OK
        )
    );
}

/// The same seam keeps a poll every second out of the request log, which is why the routes join
/// after the trace layer rather than ahead of it.
#[test]
fn test_an_availability_route_emits_no_request_span() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut log = tempfile::tempfile().unwrap();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::sync::Mutex::new(log.try_clone().unwrap()))
        .finish();

    tracing::subscriber::with_default(subscriber, || {
        runtime.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let router = build_router(&primary(&dir, RateLimitConfig::default())).unwrap();
            assert_eq!(respond(&router, HEALTH).await.status(), StatusCode::OK);
        });
    });

    let mut text = String::new();
    log.rewind().unwrap();
    log.read_to_string(&mut text).unwrap();
    let traced = text.contains(HEALTH);
    assert!(!traced);
}

/// The layer carries more than headers: it is also where a `304` loses the `Content-Length` axum
/// stamps onto it. No availability route revalidates today, so the property is pinned on the seam
/// with a route composed exactly the way the HA surface is, over HTTP/2 - the protocol that would
/// carry a length no `DATA` frame backs through to the client.
#[tokio::test]
async fn test_a_route_at_the_availability_seam_states_no_length_on_a_not_modified() {
    let dir = tempfile::tempdir().unwrap();
    let config = primary(&dir, RateLimitConfig::default());
    let state = build_state(&config).unwrap();
    let availability = ReplicationRuntime::new(&config, &state)
        .unwrap()
        .routes()
        .route("/+replication/v1/unchanged", axum::routing::get(unchanged));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let served = tokio::spawn(std::future::IntoFuture::into_future(axum::serve(
        listener,
        router_for(state, availability),
    )));

    let response = reqwest::Client::builder()
        .http2_prior_knowledge()
        .build()
        .unwrap()
        .get(format!("http://{address}/+replication/v1/unchanged"))
        .send()
        .await
        .unwrap();

    let status = response.status().as_u16();
    let stated = response.headers().contains_key(header::CONTENT_LENGTH);
    let body = response.bytes().await.unwrap();
    assert_eq!((status, stated, body.len()), (304, false, 0));
    served.abort();
}

/// A handler that states a length of its own, so the assertion covers what a route set rather than
/// only the zero an empty body measures.
async fn unchanged() -> axum::response::Response {
    (StatusCode::NOT_MODIFIED, [(header::CONTENT_LENGTH, "4096")]).into_response()
}
