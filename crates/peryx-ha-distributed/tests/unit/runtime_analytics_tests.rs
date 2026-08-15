use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::support::TestServer;
use crate::{
    AggregateDelta, AggregateKey, AggregateRow, AnalyticsBatch, AnalyticsReceiver, AuthorityEpoch,
    DEFAULT_APPLY_LIMITS, HttpAnalyticsSource, IntervalId, ProducerId, TransferLimits,
};
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::routing::get;
use axum::{Json, Router};
use http_body_util::BodyExt as _;
use peryx_events::metrics::{Clock, Metrics, Observation};
use peryx_storage::meta::MetaStore;
use tower::ServiceExt as _;

use super::*;

const TOKEN: &str = "analytics-secret";

fn open_meta() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, meta)
}

fn day_key(day: i64) -> AggregateKey {
    AggregateKey {
        day,
        repository: "example".to_owned(),
        resource: "resource-a".to_owned(),
        group: "1.0".to_owned(),
        source: String::new(),
    }
}

fn sealed_batch(day: i64, downloads: u64) -> AnalyticsBatch {
    AnalyticsBatch {
        interval: IntervalId {
            producer: ProducerId("east".to_owned()),
            epoch: AuthorityEpoch(1),
            sequence: u64::try_from(day).unwrap(),
        },
        rows: vec![AggregateRow {
            key: day_key(day),
            delta: AggregateDelta {
                downloads,
                bytes: downloads * 10,
            },
        }],
    }
}

fn canned_router(batches: Vec<AnalyticsBatch>) -> Router {
    Router::new().route(
        "/+replication/v1/analytics",
        get(move || {
            let batches = batches.clone();
            async move { Json(batches) }
        }),
    )
}

fn error_router() -> Router {
    Router::new().route(
        "/+replication/v1/analytics",
        get(|| async { axum::http::StatusCode::INTERNAL_SERVER_ERROR }),
    )
}

fn recording_router(batches: Vec<AnalyticsBatch>, after_days: Arc<Mutex<Vec<i64>>>) -> Router {
    Router::new().route(
        "/+replication/v1/analytics",
        get(move |Query(query): Query<AfterQuery>| {
            let batches = batches.clone();
            let after_days = Arc::clone(&after_days);
            async move {
                after_days.lock().unwrap().push(query.after);
                Json(batches)
            }
        }),
    )
}

fn test_puller(server: &TestServer, persist: PersistApply) -> AnalyticsPuller {
    AnalyticsPuller {
        source: HttpAnalyticsSource::new(&server.url, TOKEN, TransferLimits::default(), Duration::from_secs(30))
            .unwrap(),
        persist,
        receiver: AnalyticsReceiver::new(DEFAULT_APPLY_LIMITS),
        poll_interval: Duration::from_hours(1),
    }
}

fn analytics_request(token: Option<&str>) -> Request<Body> {
    let mut request = Request::builder().uri("/+replication/v1/analytics?after=-1");
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    request.body(Body::empty()).unwrap()
}

#[test]
fn test_resolve_producer_epoch_assigns_generation_one_on_first_start() {
    let (_dir, meta) = open_meta();
    let handle = meta.analytics();
    assert_eq!(resolve_producer_epoch(&handle).unwrap(), AuthorityEpoch(1));
    assert!(handle.load_producer().unwrap().is_some());
}

#[test]
fn test_resolve_producer_epoch_reuses_the_persisted_generation() {
    let (_dir, meta) = open_meta();
    let handle = meta.analytics();
    handle
        .save_producer(&serde_json::to_vec(&ProducerRecord { epoch: 7 }).unwrap())
        .unwrap();
    assert_eq!(resolve_producer_epoch(&handle).unwrap(), AuthorityEpoch(7));
}

#[test]
fn test_resolve_producer_epoch_reassigns_when_the_record_is_corrupt() {
    let (_dir, meta) = open_meta();
    let handle = meta.analytics();
    handle.save_producer(b"not json").unwrap();
    assert_eq!(resolve_producer_epoch(&handle).unwrap(), AuthorityEpoch(1));
}

#[test]
fn test_new_restores_a_persisted_receiver() {
    let (_dir, meta) = open_meta();
    let mut seed = AnalyticsReceiver::new(DEFAULT_APPLY_LIMITS);
    seed.apply(&sealed_batch(10, 3)).unwrap();
    meta.analytics().save_apply(&seed.encode()).unwrap();
    let puller = AnalyticsPuller::new("http://localhost:9/", TOKEN, meta.analytics(), Duration::from_hours(1)).unwrap();
    assert_eq!(puller.receiver.resume_day(), 10);
}

#[test]
fn test_new_rejects_an_unusable_upstream() {
    let (_dir, meta) = open_meta();
    let built = AnalyticsPuller::new("not a url", TOKEN, meta.analytics(), Duration::from_hours(1));
    assert!(built.is_err());
}

#[test]
fn test_new_surfaces_a_corrupt_apply_snapshot() {
    let (_dir, meta) = open_meta();
    meta.analytics().save_apply(b"not json").unwrap();
    let built = AnalyticsPuller::new("http://localhost:9/", TOKEN, meta.analytics(), Duration::from_hours(1));
    assert!(built.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_endpoint_rejects_an_unauthenticated_pull() {
    let router = analytics_router(
        TOKEN,
        Arc::new(Metrics::start()),
        ProducerId("east".to_owned()),
        AuthorityEpoch(1),
    );
    let response = router.oneshot(analytics_request(None)).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_endpoint_rejects_a_mismatched_token() {
    let router = analytics_router(
        TOKEN,
        Arc::new(Metrics::start()),
        ProducerId("east".to_owned()),
        AuthorityEpoch(1),
    );
    let response = router.oneshot(analytics_request(Some("wrong"))).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_endpoint_serves_sealed_batches_to_an_authorized_pull() {
    let day = Arc::new(AtomicI64::new(10 * 86_400));
    let clock_day = Arc::clone(&day);
    let clock: Clock = Arc::new(move || clock_day.load(Ordering::SeqCst));
    let (_dir, meta) = open_meta();
    let metrics = Metrics::start_durable(meta.analytics(), None, clock).unwrap();
    metrics.record(Observation::Read {
        repository: "example".to_owned(),
        resource: "resource-a".to_owned(),
        artifact: "artifact-a.bin".to_owned(),
        group: Some("1.0".to_owned()),
        source: None,
        bytes: 100,
    });
    metrics.flush().unwrap();
    day.store(11 * 86_400, Ordering::SeqCst);

    let response = analytics_router(
        TOKEN,
        Arc::new(metrics),
        ProducerId("east".to_owned()),
        AuthorityEpoch(1),
    )
    .oneshot(analytics_request(Some(TOKEN)))
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let batches: Vec<AnalyticsBatch> =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].interval.sequence, 10);
    assert_eq!(batches[0].rows[0].delta.downloads, 1);
    assert_eq!(batches[0].rows[0].delta.bytes, 100);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_pull_once_persists_a_pulled_batch() {
    let server = TestServer::start(canned_router(vec![sealed_batch(10, 3)])).await;
    let (_dir, meta) = open_meta();
    let mut puller = AnalyticsPuller::new(&server.url, TOKEN, meta.analytics(), Duration::from_hours(1)).unwrap();
    puller.pull_once().await;
    assert_eq!(puller.receiver.total(&day_key(10)).downloads, 3);
    assert!(meta.analytics().load_apply().unwrap().is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_pull_once_ignores_an_empty_pull() {
    let server = TestServer::start(canned_router(Vec::new())).await;
    let (_dir, meta) = open_meta();
    let mut puller = AnalyticsPuller::new(&server.url, TOKEN, meta.analytics(), Duration::from_hours(1)).unwrap();
    puller.pull_once().await;
    assert!(meta.analytics().load_apply().unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_pull_once_survives_a_transport_loss() {
    let server = TestServer::start(error_router()).await;
    let (_dir, meta) = open_meta();
    let mut puller = AnalyticsPuller::new(&server.url, TOKEN, meta.analytics(), Duration::from_hours(1)).unwrap();
    puller.pull_once().await;
    assert!(meta.analytics().load_apply().unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_pull_once_retries_the_same_cursor_after_a_transient_persist_failure() {
    let after_days = Arc::new(Mutex::new(Vec::new()));
    let server = TestServer::start(recording_router(vec![sealed_batch(10, 3)], Arc::clone(&after_days))).await;
    let (_dir, meta) = open_meta();
    let handle = meta.analytics();
    let attempts = Arc::new(AtomicUsize::new(0));
    let persist_attempts = Arc::clone(&attempts);
    let stored = handle.clone();
    let mut puller = test_puller(
        &server,
        Box::new(move |snapshot| {
            if persist_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(anyhow::anyhow!("store offline"));
            }
            stored.save_apply(snapshot).map_err(anyhow::Error::from)
        }),
    );

    puller.pull_once().await;
    assert_eq!(puller.receiver.total(&day_key(10)).downloads, 0);
    assert!(handle.load_apply().unwrap().is_none());

    puller.pull_once().await;
    assert_eq!(puller.receiver.total(&day_key(10)).downloads, 3);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(*after_days.lock().unwrap(), [-1, -1]);
    let snapshot = handle.load_apply().unwrap().unwrap();
    let persisted = AnalyticsReceiver::restore(&snapshot, DEFAULT_APPLY_LIMITS).unwrap();
    assert_eq!(persisted.total(&day_key(10)).downloads, 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_pull_once_keeps_the_cursor_and_state_unchanged_across_repeated_persist_failures() {
    let after_days = Arc::new(Mutex::new(Vec::new()));
    let server = TestServer::start(recording_router(vec![sealed_batch(10, 3)], Arc::clone(&after_days))).await;
    let mut puller = test_puller(&server, Box::new(|_| Err(anyhow::anyhow!("store offline"))));

    puller.pull_once().await;
    assert_eq!(puller.receiver.total(&day_key(10)).downloads, 0);
    puller.pull_once().await;
    assert_eq!(puller.receiver.total(&day_key(10)).downloads, 0);
    assert_eq!(*after_days.lock().unwrap(), [-1, -1]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_run_persists_each_iteration_until_cancelled() {
    let server = TestServer::start(canned_router(vec![sealed_batch(10, 3)])).await;
    let (_dir, meta) = open_meta();
    let handle = meta.analytics();
    let source =
        HttpAnalyticsSource::new(&server.url, TOKEN, TransferLimits::default(), Duration::from_secs(30)).unwrap();
    let (persisted_tx, persisted_rx) = tokio::sync::oneshot::channel();
    let persisted_tx = Mutex::new(Some(persisted_tx));
    let stored = handle.clone();
    let puller = AnalyticsPuller {
        source,
        persist: Box::new(move |snapshot| {
            stored.save_apply(snapshot).map_err(anyhow::Error::from)?;
            let sender = persisted_tx.lock().unwrap().take();
            if let Some(sender) = sender {
                let _ = sender.send(());
            }
            Ok(())
        }),
        receiver: AnalyticsReceiver::new(DEFAULT_APPLY_LIMITS),
        poll_interval: Duration::from_hours(1),
    };
    let task = tokio::spawn(puller.run());

    tokio::time::timeout(Duration::from_secs(5), persisted_rx)
        .await
        .unwrap()
        .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());

    assert!(handle.load_apply().unwrap().is_some());
}
