use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::process::Output;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{Response, StatusCode, Uri, header};
use axum::routing::put;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use peryx_ecosystem_pypi::store::PypiStore as _;
use peryx_storage::blob::Digest;
use peryx_storage::meta::MetaStore;
use sha2::Digest as _;
use tokio::process::Command;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

const BUCKET: &str = "peryx-tests";
const FILENAME: &str = "veloxdemo-1.0.0-py3-none-any.whl";
const WHEEL: &[u8] = include_bytes!("../../../fixtures/veloxdemo-1.0.0-py3-none-any.whl");

struct S3Server {
    endpoint: String,
    objects: Arc<Mutex<Vec<StoredObject>>>,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl S3Server {
    async fn stop(mut self) {
        self.shutdown.take().unwrap().send(()).unwrap();
        self.task.take().unwrap().await.unwrap();
    }
}

impl Drop for S3Server {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

struct PendingTask {
    started: Option<oneshot::Sender<()>>,
    aborted: Option<oneshot::Sender<()>>,
}

impl Future for PendingTask {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
        if let Some(started) = self.started.take() {
            started.send(()).unwrap();
        }
        Poll::Pending
    }
}

impl Drop for PendingTask {
    fn drop(&mut self) {
        self.aborted.take().unwrap().send(()).unwrap();
    }
}

#[tokio::test]
async fn test_s3_server_drop_signals_shutdown_and_aborts_task() {
    let (shutdown, stopped) = oneshot::channel();
    let (started_tx, started_rx) = oneshot::channel();
    let (aborted_tx, aborted_rx) = oneshot::channel();
    let task = tokio::spawn(PendingTask {
        started: Some(started_tx),
        aborted: Some(aborted_tx),
    });
    started_rx.await.unwrap();
    drop(S3Server {
        endpoint: String::new(),
        objects: Arc::new(Mutex::new(Vec::new())),
        shutdown: Some(shutdown),
        task: Some(task),
    });
    stopped.await.unwrap();
    aborted_rx.await.unwrap();
}

#[derive(Debug, PartialEq, Eq)]
struct StoredObject {
    path: String,
    bytes: Vec<u8>,
    if_none_match: String,
    checksum: String,
}

async fn s3_server() -> S3Server {
    let objects = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (shutdown, stopped) = oneshot::channel();
    let task = tokio::spawn({
        let objects = Arc::clone(&objects);
        async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/{*key}", put(store_object).head(head_object))
                    .with_state(objects),
            )
            .with_graceful_shutdown(async { stopped.await.unwrap() })
            .await
            .unwrap();
        }
    });
    S3Server {
        endpoint,
        objects,
        shutdown: Some(shutdown),
        task: Some(task),
    }
}

async fn store_object(State(objects): State<Arc<Mutex<Vec<StoredObject>>>>, request: Request) -> StatusCode {
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, WHEEL.len()).await.unwrap().to_vec();
    objects.lock().unwrap().push(StoredObject {
        path: parts.uri.path().to_owned(),
        bytes,
        if_none_match: parts.headers[header::IF_NONE_MATCH].to_str().unwrap().to_owned(),
        checksum: parts.headers["x-amz-checksum-sha256"].to_str().unwrap().to_owned(),
    });
    StatusCode::OK
}

async fn head_object(State(objects): State<Arc<Mutex<Vec<StoredObject>>>>, uri: Uri) -> Response<Body> {
    let content_length = objects
        .lock()
        .unwrap()
        .iter()
        .find(|object| object.path == uri.path())
        .unwrap()
        .bytes
        .len();
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_LENGTH, content_length)
        .body(Body::empty())
        .unwrap()
}

async fn child(endpoint: &str, data_dir: &Path) -> Output {
    tokio::time::timeout(
        Duration::from_secs(30),
        Command::new(env!("CARGO_BIN_EXE_peryx-pypi-s3-upload-fixture"))
            .arg("upload-orphan")
            .arg(endpoint)
            .arg(data_dir)
            .env("AWS_ACCESS_KEY_ID", "peryx-s3-test")
            .env("AWS_SECRET_ACCESS_KEY", "peryx-s3-test-secret")
            .env("AWS_REGION", "us-east-1")
            .env("AWS_EC2_METADATA_DISABLED", "true")
            .env_remove("AWS_PROFILE")
            .env_remove("AWS_SHARED_CREDENTIALS_FILE")
            .output(),
    )
    .await
    .unwrap()
    .unwrap()
}

#[tokio::test]
async fn test_s3_upload_metadata_failure_leaves_a_detectable_orphan() {
    let server = s3_server().await;
    let data_dir = tempfile::tempdir().unwrap();
    MetaStore::open(data_dir.path().join("peryx.redb"))
        .unwrap()
        .put_upload("hosted", "veloxdemo", FILENAME, b"invalid-json")
        .unwrap();
    let output = child(&server.endpoint, data_dir.path()).await;
    assert_eq!(
        (output.status.success(), output.stdout, output.stderr),
        (true, Vec::new(), Vec::new())
    );
    let expected = StoredObject {
        path: format!("/{BUCKET}/cache/sha256/{}", Digest::of(WHEEL).as_str()),
        bytes: WHEEL.to_vec(),
        if_none_match: "*".to_owned(),
        checksum: STANDARD.encode(sha2::Sha256::digest(WHEEL)),
    };
    assert_eq!(
        server
            .objects
            .lock()
            .unwrap()
            .iter()
            .find(|object| object.path == expected.path)
            .unwrap(),
        &expected
    );
    server.stop().await;
}
