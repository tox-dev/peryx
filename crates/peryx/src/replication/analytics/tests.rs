use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use axum::routing::get;
use axum::{Json, Router};
use peryx_events::metrics::{Clock, Event, Metrics};
use peryx_ha_distributed::{
    AggregateDelta, AggregateKey, AggregateRow, AnalyticsBatch, AnalyticsReceiver, AuthorityEpoch,
    DEFAULT_APPLY_LIMITS, HttpAnalyticsSource, IntervalId, ProducerId, TransferLimits,
};
use peryx_storage::meta::MetaStore;

use super::*;

const TOKEN: &str = "analytics-secret";

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

fn open_meta() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, meta)
}

fn day_key(day: i64) -> AggregateKey {
    AggregateKey {
        day,
        repository: "pypi".to_owned(),
        project: "flask".to_owned(),
        version: "1.0".to_owned(),
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
        Metrics::start(),
        ProducerId("east".to_owned()),
        AuthorityEpoch(1),
    );
    let server = TestServer::start(router).await;
    let response = reqwest::Client::new()
        .get(format!("{}+replication/v1/analytics?after=-1", server.url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_endpoint_rejects_a_mismatched_token() {
    let router = analytics_router(
        TOKEN,
        Metrics::start(),
        ProducerId("east".to_owned()),
        AuthorityEpoch(1),
    );
    let server = TestServer::start(router).await;
    let response = reqwest::Client::new()
        .get(format!("{}+replication/v1/analytics?after=-1", server.url))
        .bearer_auth("wrong")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_endpoint_serves_sealed_batches_to_an_authorized_pull() {
    let day = Arc::new(AtomicI64::new(10 * 86_400));
    let clock_day = Arc::clone(&day);
    let clock: Clock = Arc::new(move || clock_day.load(Ordering::SeqCst));
    let (_dir, meta) = open_meta();
    let metrics = Metrics::start_durable(meta.analytics(), None, clock);
    metrics.record(Event::Download {
        route: "pypi".to_owned(),
        project: "flask".to_owned(),
        filename: "flask-1.0.whl".to_owned(),
        version: Some("1.0".to_owned()),
        source: None,
        bytes: 100,
    });
    metrics.settle();
    day.store(11 * 86_400, Ordering::SeqCst);

    let router = analytics_router(TOKEN, metrics, ProducerId("east".to_owned()), AuthorityEpoch(1));
    let server = TestServer::start(router).await;
    let response = reqwest::Client::new()
        .get(format!("{}+replication/v1/analytics?after=-1", server.url))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let batches: Vec<AnalyticsBatch> = response.json().await.unwrap();
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
async fn test_pull_once_logs_a_persist_failure_without_stopping() {
    let server = TestServer::start(canned_router(vec![sealed_batch(10, 3)])).await;
    let source =
        HttpAnalyticsSource::new(&server.url, TOKEN, TransferLimits::default(), Duration::from_secs(30)).unwrap();
    let mut puller = AnalyticsPuller {
        source,
        persist: Box::new(|_| Err(anyhow::anyhow!("store offline"))),
        receiver: AnalyticsReceiver::new(DEFAULT_APPLY_LIMITS),
        poll_interval: Duration::from_hours(1),
    };
    puller.pull_once().await;
    assert_eq!(puller.receiver.total(&day_key(10)).downloads, 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_run_persists_each_iteration_until_cancelled() {
    let server = TestServer::start(canned_router(vec![sealed_batch(10, 3)])).await;
    let (_dir, meta) = open_meta();
    let handle = meta.analytics();
    let puller = AnalyticsPuller::new(&server.url, TOKEN, handle.clone(), Duration::from_millis(10)).unwrap();
    let task = tokio::spawn(puller.run());
    loop {
        if handle.load_apply().unwrap().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    task.abort();
    let _ = task.await;
    assert!(handle.load_apply().unwrap().is_some());
}
