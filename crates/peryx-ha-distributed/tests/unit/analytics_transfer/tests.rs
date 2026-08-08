use std::num::{NonZeroU64, NonZeroUsize};
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::Query;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};

use super::{AnalyticsPullError, AnalyticsSource, HttpAnalyticsError, HttpAnalyticsSource, pull};
use crate::AuthorityEpoch;
use crate::analytics::{
    AggregateDelta, AggregateKey, AggregateRow, AnalyticsBatch, AnalyticsReceiver, ApplyLimits, DEFAULT_APPLY_LIMITS,
    IntervalId, ProducerId,
};
use crate::peer::{TransferLimits, TransportError};

const TOKEN: &str = "secret";

fn limits(max_encoded_bytes: u64) -> TransferLimits {
    TransferLimits {
        max_operations: NonZeroUsize::new(256).unwrap(),
        max_encoded_bytes: NonZeroU64::new(max_encoded_bytes).unwrap(),
    }
}

fn producer() -> ProducerId {
    ProducerId("east".to_owned())
}

fn batch(day: i64, downloads: u64) -> AnalyticsBatch {
    AnalyticsBatch {
        interval: IntervalId {
            producer: producer(),
            epoch: AuthorityEpoch(1),
            sequence: u64::try_from(day).unwrap(),
        },
        rows: vec![AggregateRow {
            key: AggregateKey {
                day,
                repository: "alpha".to_owned(),
                project: "flask".to_owned(),
                version: "1.0".to_owned(),
                source: String::new(),
            },
            delta: AggregateDelta {
                downloads,
                bytes: downloads * 10,
            },
        }],
    }
}

struct Canned(Result<Vec<AnalyticsBatch>, TransportError>);

#[async_trait]
impl AnalyticsSource for Canned {
    async fn fetch_after(&self, _after_day: i64) -> Result<Vec<AnalyticsBatch>, TransportError> {
        self.0.clone()
    }
}

#[tokio::test]
async fn test_pull_folds_new_batches_then_dedups_a_re_pull() {
    let source = Canned(Ok(vec![batch(10, 3), batch(11, 4)]));
    let mut receiver = AnalyticsReceiver::new(DEFAULT_APPLY_LIMITS);

    let first = pull(&source, &mut receiver).await.unwrap();
    assert_eq!((first.applied, first.duplicate), (2, 0));
    assert_eq!(receiver.after_day(&producer()), 11);
    assert_eq!(
        receiver.total(&batch(10, 3).rows[0].key),
        AggregateDelta {
            downloads: 3,
            bytes: 30
        }
    );

    // A re-pull of the same batches is recognized and folds nothing further.
    let second = pull(&source, &mut receiver).await.unwrap();
    assert_eq!((second.applied, second.duplicate), (0, 2));
    assert_eq!(
        receiver.total(&batch(10, 3).rows[0].key),
        AggregateDelta {
            downloads: 3,
            bytes: 30
        }
    );
}

#[tokio::test]
async fn test_pull_surfaces_a_transport_loss() {
    let source = Canned(Err(TransportError::Timeout));
    let mut receiver = AnalyticsReceiver::new(DEFAULT_APPLY_LIMITS);

    let error = pull(&source, &mut receiver).await.unwrap_err();

    assert!(matches!(error, AnalyticsPullError::Transport(TransportError::Timeout)));
}

#[tokio::test]
async fn test_pull_surfaces_an_apply_bound_breach() {
    let source = Canned(Ok(vec![batch(10, 1), batch(11, 1)]));
    let mut receiver = AnalyticsReceiver::new(ApplyLimits {
        max_rows_per_batch: 16,
        max_retained_intervals: 1,
    });

    let error = pull(&source, &mut receiver).await.unwrap_err();

    assert!(matches!(error, AnalyticsPullError::Apply(_)), "{error:?}");
    // The first batch applied; the second breached the retention bound.
    assert_eq!(receiver.after_day(&producer()), 10);
}

#[test]
fn test_http_source_rejects_an_empty_token() {
    let error =
        HttpAnalyticsSource::new("http://producer:8080/", "", limits(1024), Duration::from_secs(5)).unwrap_err();
    assert!(matches!(error, HttpAnalyticsError::EmptyToken));
}

#[test]
fn test_http_source_rejects_a_non_http_base() {
    let error = HttpAnalyticsSource::new("ftp://producer/", TOKEN, limits(1024), Duration::from_secs(5)).unwrap_err();
    assert!(matches!(error, HttpAnalyticsError::InvalidBase(_)));
}

#[test]
fn test_http_source_rejects_an_unparseable_base() {
    let error = HttpAnalyticsSource::new("not a url", TOKEN, limits(1024), Duration::from_secs(5)).unwrap_err();
    assert!(matches!(error, HttpAnalyticsError::InvalidBase(_)));
}

#[test]
fn test_http_source_appends_a_trailing_slash_to_the_base() {
    // A path base without a trailing slash still roots the analytics path rather than dropping a segment.
    assert!(
        HttpAnalyticsSource::new(
            "http://producer:8080/prefix",
            TOKEN,
            limits(1024),
            Duration::from_secs(5)
        )
        .is_ok()
    );
}

#[test]
fn test_http_source_debug_redacts_the_token() {
    let source =
        HttpAnalyticsSource::new("http://producer:8080/", TOKEN, limits(1024), Duration::from_secs(5)).unwrap();
    let rendered = format!("{source:?}");
    assert!(rendered.contains("<redacted>"), "{rendered}");
    assert!(!rendered.contains(TOKEN), "{rendered}");
    assert!(rendered.contains("producer"), "{rendered}");
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

fn source(url: &str) -> HttpAnalyticsSource {
    HttpAnalyticsSource::new(url, TOKEN, limits(1 << 20), Duration::from_secs(5)).unwrap()
}

#[derive(serde::Deserialize)]
struct After {
    after: i64,
}

#[tokio::test]
async fn test_http_source_fetches_and_parses_batches() {
    let router = Router::new().route(
        "/+replication/v1/analytics",
        get(|headers: HeaderMap, Query(after): Query<After>| async move {
            assert_eq!(headers.get(header::AUTHORIZATION).unwrap(), "Bearer secret");
            assert_eq!(after.after, -1);
            Json(vec![batch(10, 3)])
        }),
    );
    let server = TestServer::start(router).await;

    let batches = source(&server.url).fetch_after(-1).await.unwrap();

    assert_eq!(batches, vec![batch(10, 3)]);
}

async fn status_server(status: StatusCode) -> TestServer {
    TestServer::start(Router::new().route(
        "/+replication/v1/analytics",
        get(move || async move { status.into_response() }),
    ))
    .await
}

#[tokio::test]
async fn test_http_source_maps_unauthorized() {
    let server = status_server(StatusCode::UNAUTHORIZED).await;
    let error = source(&server.url).fetch_after(0).await.unwrap_err();
    assert!(matches!(error, TransportError::Unauthenticated));
}

#[tokio::test]
async fn test_http_source_maps_a_server_error() {
    let server = status_server(StatusCode::BAD_GATEWAY).await;
    let error = source(&server.url).fetch_after(0).await.unwrap_err();
    assert!(matches!(error, TransportError::ServerError { status: 502 }));
}

#[tokio::test]
async fn test_http_source_maps_an_unexpected_status() {
    let server = status_server(StatusCode::NOT_FOUND).await;
    let error = source(&server.url).fetch_after(0).await.unwrap_err();
    assert!(matches!(error, TransportError::BadStatus { status: 404 }));
}

#[tokio::test]
async fn test_http_source_rejects_a_malformed_body() {
    let server =
        TestServer::start(Router::new().route("/+replication/v1/analytics", get(|| async { "not a batch list" })))
            .await;
    let error = source(&server.url).fetch_after(0).await.unwrap_err();
    assert!(matches!(error, TransportError::Malformed));
}

#[tokio::test]
async fn test_http_source_rejects_an_oversized_body() {
    let router = Router::new().route("/+replication/v1/analytics", get(|| async { Json(vec![batch(10, 3)]) }));
    let server = TestServer::start(router).await;
    let tiny = HttpAnalyticsSource::new(&server.url, TOKEN, limits(4), Duration::from_secs(5)).unwrap();

    let error = tiny.fetch_after(0).await.unwrap_err();

    assert!(matches!(error, TransportError::FrameTooLarge { .. }));
}

#[tokio::test]
async fn test_http_source_maps_a_refused_connection() {
    // Bind then drop, so the port is closed and the connection is refused.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    drop(listener);

    let error = source(&url).fetch_after(0).await.unwrap_err();

    assert!(matches!(error, TransportError::Disconnected | TransportError::Timeout));
}

#[tokio::test]
async fn test_http_source_maps_a_response_timeout() {
    // A listener that accepts but never answers holds the request open until the client's timeout fires.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        // Accept the connection and hold it open, never answering, until the task is aborted.
        let _connection = listener.accept().await;
        std::future::pending::<()>().await;
    });
    let silent = HttpAnalyticsSource::new(&url, TOKEN, limits(1 << 20), Duration::from_millis(150)).unwrap();

    let error = silent.fetch_after(0).await.unwrap_err();

    assert!(matches!(error, TransportError::Timeout), "{error:?}");
    task.abort();
}

#[tokio::test]
async fn test_http_source_maps_a_truncated_body() {
    use tokio::io::AsyncWriteExt as _;

    // Announce a long body, then close after a single byte so the streamed read fails mid-body.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 4096\r\n\r\n[")
                .await;
            let _ = stream.flush().await;
            // Drop the connection before the promised bytes arrive.
        }
    });

    let error = source(&url).fetch_after(0).await.unwrap_err();

    assert!(
        matches!(error, TransportError::Disconnected | TransportError::Timeout),
        "{error:?}"
    );
    task.abort();
}
