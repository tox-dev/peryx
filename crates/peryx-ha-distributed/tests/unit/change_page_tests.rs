use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use http_body_util::BodyExt as _;
use peryx_driver::{BlockingScanExecutor, ScanCancellation};
use peryx_storage::blob::BlobStorage;
use peryx_storage::meta::{MetaError, MetaStore};
use tower::ServiceExt as _;

use super::{ChangePageBody, build_change_page, change_page_response};
use crate::protocol::{Change, ChangePage, PROTOCOL_VERSION};
use crate::{DEFAULT_MAX_CONCURRENT_BLOB_STREAMS, follower_router_with_change_pages, primary_router_with_limits};

const TOKEN: &str = "replica-secret";

#[tokio::test]
async fn test_a_page_carries_the_records_after_its_cursor() {
    let (_dir, meta) = journaled(&[b"one", b"two", b"three"]);

    let (status, body) = served(build_change_page(&meta, "primary-a", 1, 10, &ScanCancellation::new())).await;

    assert_eq!(
        (status, serde_json::from_slice::<ChangePage>(&body).unwrap()),
        (
            StatusCode::OK,
            ChangePage {
                version: PROTOCOL_VERSION,
                source: "primary-a".to_owned(),
                after: 1,
                current_serial: 3,
                changes: vec![change(2, b"two"), change(3, b"three"),],
            }
        )
    );
}

#[tokio::test]
async fn test_a_cancelled_build_stops_instead_of_encoding_a_page() {
    let (_dir, meta) = journaled(&[b"one", b"two"]);
    let cancellation = ScanCancellation::new();
    cancellation.cancel();

    let (status, body) = served(build_change_page(&meta, "primary-a", 0, 10, &cancellation)).await;

    assert_eq!(
        (status, String::from_utf8(body).unwrap()),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "change page build stopped early".to_owned()
        )
    );
}

#[tokio::test]
async fn test_a_failed_journal_read_reports_a_server_error() {
    let dir = tempfile::tempdir().unwrap();

    let (status, _) = served(build_change_page(
        &unreadable_store(&dir),
        "primary-a",
        0,
        10,
        &ScanCancellation::new(),
    ))
    .await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn test_a_panicking_page_worker_reports_a_server_error() {
    let panicked = tokio::task::spawn_blocking(|| panic!("change page worker panic"))
        .await
        .unwrap_err();

    assert_eq!(
        change_page_response(Err(panicked)).status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_a_primary_refuses_a_page_before_it_reads_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let pages = BlockingScanExecutor::new(1);
    let router = primary_router_with_limits(
        "primary-a",
        TOKEN,
        unreadable_store(&dir),
        BlobStorage::filesystem(dir.path().join("blobs")),
        DEFAULT_MAX_CONCURRENT_BLOB_STREAMS,
        pages.clone(),
    )
    .unwrap();
    let saturated = SaturatedPages::start(&pages, 1).await;

    let response = router
        .oneshot(authenticated("/+replication/v1/changes?after=0&limit=1"))
        .await
        .unwrap();
    let (status, retry_after) = (response.status(), response.headers()[header::RETRY_AFTER].clone());
    let body = response.into_body().collect().await.unwrap().to_bytes();
    saturated.release().await;

    assert_eq!(
        (
            status,
            retry_after.to_str().unwrap(),
            String::from_utf8(body.to_vec()).unwrap()
        ),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "1",
            "peer change page capacity reached".to_owned()
        )
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_a_follower_refuses_a_page_before_it_reads_its_replica_state() {
    let dir = tempfile::tempdir().unwrap();
    let pages = BlockingScanExecutor::new(1);
    let router = follower_router_with_change_pages(TOKEN, unreadable_store(&dir), pages.clone()).unwrap();
    let saturated = SaturatedPages::start(&pages, 1).await;

    let response = router
        .oneshot(authenticated("/+replication/v1/changes?after=0&limit=1"))
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    saturated.release().await;

    assert_eq!(
        (status, String::from_utf8(body.to_vec()).unwrap()),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "peer change page capacity reached".to_owned()
        )
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_a_saturated_change_feed_still_serves_an_artifact() {
    let (dir, meta) = journaled(&[b"one"]);
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let digest = blobs.put_bytes(b"artifact bytes").await.unwrap();
    let pages = BlockingScanExecutor::new(1);
    let router = primary_router_with_limits(
        "primary-a",
        TOKEN,
        meta,
        blobs,
        DEFAULT_MAX_CONCURRENT_BLOB_STREAMS,
        pages.clone(),
    )
    .unwrap();
    let saturated = SaturatedPages::start(&pages, 1).await;

    let response = router
        .oneshot(authenticated(&format!(
            "/+replication/v1/blobs/sha256/{}",
            digest.as_str()
        )))
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    saturated.release().await;

    assert_eq!((status, body.as_ref()), (StatusCode::OK, b"artifact bytes".as_slice()));
}

fn change(serial: u64, event: &[u8]) -> Change {
    Change {
        serial,
        event: event.to_vec(),
        metadata: Vec::new(),
        blobs: Vec::new(),
    }
}

async fn served(built: ChangePageBody) -> (StatusCode, Vec<u8>) {
    let response: Response = change_page_response(Ok(built));
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, body.to_vec())
}

fn journaled(values: &[&[u8]]) -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    meta.commit_driver_txn(|_| Ok::<_, MetaError>(((), values.iter().map(|value| value.to_vec()).collect())))
        .unwrap();
    (dir, meta)
}

/// Opens a store whose serial table is missing, so any journal or replica read fails.
fn unreadable_store(dir: &tempfile::TempDir) -> MetaStore {
    let path = dir.path().join("empty.redb");
    drop(redb::Database::create(&path).unwrap());
    MetaStore::open_existing(path).unwrap()
}

fn authenticated(uri: &str) -> Request<Body> {
    Request::get(uri)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .body(Body::empty())
        .unwrap()
}

/// Occupies every change-page slot until the test releases the workers.
///
/// A flag the worker rechecks under a condition variable only makes it wait when the test loses the
/// race to set that flag, so the wait runs or not depending on thread timing. Each worker instead
/// blocks on a channel receive, which runs on every pass whether or not the release already landed.
struct SaturatedPages {
    open: Sender<()>,
    workers: Vec<tokio::task::JoinHandle<Option<Result<(), tokio::task::JoinError>>>>,
}

impl SaturatedPages {
    async fn start(executor: &BlockingScanExecutor, slots: usize) -> Self {
        let (open, reached) = channel();
        let reached = Arc::new(Mutex::new(reached));
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut workers = Vec::new();
        for _ in 0..slots {
            let executor = executor.clone();
            let reached = reached.clone();
            let started_tx = started_tx.clone();
            workers.push(tokio::spawn(async move {
                executor
                    .try_run(move |_| {
                        started_tx.send(()).unwrap();
                        // Either the token or a disconnect frees this worker, and only a failing test disconnects.
                        let _ = reached.lock().unwrap().recv();
                    })
                    .await
            }));
        }
        for _ in 0..slots {
            started_rx.recv().await.unwrap();
        }
        Self { open, workers }
    }

    async fn release(self) {
        for _ in 0..self.workers.len() {
            self.open.send(()).unwrap();
        }
        for worker in self.workers {
            worker.await.unwrap().unwrap().unwrap();
        }
    }
}
