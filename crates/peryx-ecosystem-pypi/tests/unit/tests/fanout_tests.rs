use std::error::Error as _;
use std::sync::Arc;

use bytes::Bytes;
use futures_util::StreamExt as _;
use peryx_storage::blob::{BlobError, BlobStorage, Digest};
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use super::http::{LocalDurability, UnavailableCompleteness, detail_json, get, harness};
use crate::cache;
use crate::store::PypiStore as _;
use peryx_driver::download::DownloadHandle;
use peryx_driver::state::{AppState, ServingState};

struct TestUpstream {
    url: String,
    address: std::net::SocketAddr,
    release: std::sync::mpsc::Sender<()>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl TestUpstream {
    fn release(&self) {
        self.release.send(()).unwrap();
    }
}

impl Drop for TestUpstream {
    fn drop(&mut self) {
        let _ = self.release.send(());
        let _ = std::net::TcpStream::connect(self.address);
        let joined = self.handle.take().unwrap().join();
        if !std::thread::panicking() {
            joined.expect("upstream fixture panicked");
        }
    }
}

fn stalling_upstream(first: Vec<u8>, rest: Vec<u8>) -> TestUpstream {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let handle = std::thread::spawn(move || {
        use std::io::{Read as _, Write as _};
        let (mut socket, _) = listener.accept().unwrap();
        let mut buffer = [0u8; 1024];
        let _ = socket.read(&mut buffer);
        let header = format!(
            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n",
            first.len() + rest.len()
        );
        socket.write_all(header.as_bytes()).unwrap();
        socket.write_all(&first).unwrap();
        socket.flush().unwrap();
        released.recv().unwrap();
        socket.write_all(&rest).unwrap();
    });
    TestUpstream {
        url: format!("http://{addr}/stalled.whl"),
        address: addr,
        release,
        handle: Some(handle),
    }
}

fn truncated_upstream() -> TestUpstream {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let handle = std::thread::spawn(move || {
        use std::io::{Read as _, Write as _};
        let (mut socket, _) = listener.accept().unwrap();
        let mut buffer = [0u8; 1024];
        let _ = socket.read(&mut buffer);
        socket
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\npart")
            .unwrap();
        socket.flush().unwrap();
        released.recv().unwrap();
    });
    TestUpstream {
        url: format!("http://{addr}/truncated.whl"),
        address: addr,
        release,
        handle: Some(handle),
    }
}

async fn live_stream_for(state: &Arc<ServingState>, digest: &Digest) -> cache::FileOutcome {
    cache::stream_file(
        state.clone(),
        digest.clone(),
        "pypi".to_owned(),
        "stalled.whl".to_owned(),
    )
    .await
    .unwrap()
}

fn live_stream(
    outcome: cache::FileOutcome,
) -> Option<futures_util::stream::BoxStream<'static, Result<Bytes, std::io::Error>>> {
    match outcome {
        cache::FileOutcome::Live(stream) => Some(stream),
        cache::FileOutcome::Cached(_) => None,
    }
}

async fn transfer_result(handle: &mut DownloadHandle) -> Result<(), String> {
    loop {
        let done = handle.progress().borrow_and_update().done.clone();
        if let Some(result) = done {
            return result;
        }
        handle
            .progress()
            .changed()
            .await
            .expect("the transfer producer publishes a terminal result");
    }
}

#[tokio::test]
async fn test_concurrent_cold_requests_stream_before_the_transfer_finishes() {
    let h = harness().await;

    let first = vec![0xAAu8; 400 * 1024];
    let rest = vec![0xBBu8; 300 * 1024];
    let mut whole = first.clone();
    whole.extend_from_slice(&rest);
    let digest = Digest::of(&whole);
    let upstream = stalling_upstream(first.clone(), rest);
    h.state
        .serving
        .meta
        .put_file_url(digest.as_str(), &upstream.url, "pypi")
        .unwrap();

    let mut leader = live_stream(live_stream_for(&h.state.serving, &digest).await).expect("a live stream");
    let mut follower = live_stream(live_stream_for(&h.state.serving, &digest).await)
        .expect("the follower to attach to the live transfer");

    let leader_first = leader.next().await.unwrap().unwrap();
    let follower_first = follower.next().await.unwrap().unwrap();
    assert!(!leader_first.is_empty());
    assert!(!follower_first.is_empty());

    upstream.release();
    for (stream, mut body) in [
        (&mut leader, leader_first.to_vec()),
        (&mut follower, follower_first.to_vec()),
    ] {
        while let Some(chunk) = stream.next().await {
            body.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(body, whole);
    }
    assert!(h.state.serving.blobs.head(&digest).await.unwrap().is_some());
}

#[tokio::test]
async fn test_client_arriving_after_commit_streams_from_disk() {
    let h = harness().await;
    let body = vec![0xCCu8; 8 * 1024];
    let digest = Digest::of(&body);
    let upstream = stalling_upstream(body[..4096].to_vec(), body[4096..].to_vec());
    h.state
        .serving
        .meta
        .put_file_url(digest.as_str(), &upstream.url, "pypi")
        .unwrap();
    let mut leader = live_stream(live_stream_for(&h.state.serving, &digest).await).expect("a live stream");
    upstream.release();
    let mut streamed = Vec::new();
    while let Some(chunk) = leader.next().await {
        streamed.extend_from_slice(&chunk.unwrap());
    }
    assert_eq!(streamed, body);
    assert!(live_stream(live_stream_for(&h.state.serving, &digest).await).is_none());
}

#[tokio::test]
async fn test_blob_committed_while_waiting_on_the_gate_serves_from_disk() {
    let h = harness().await;
    let body = b"landed while parked";
    let digest = Digest::of(body);
    let gate = cache::flight_gate(&h.state.serving, digest.as_str());
    let guard = gate.lock_owned().await;
    let waiting = cache::stream_file(
        h.state.serving.clone(),
        digest.clone(),
        "pypi".to_owned(),
        "parked.whl".to_owned(),
    );
    tokio::pin!(waiting);
    assert!(futures_util::poll!(waiting.as_mut()).is_pending());
    h.state.serving.blobs.put_bytes_as(body, &digest).await.unwrap();
    drop(guard);
    let outcome = waiting.await.unwrap();
    assert!(matches!(outcome, cache::FileOutcome::Cached(_)));
}

#[tokio::test]
async fn test_digest_mismatch_fails_every_tail_and_persists_nothing() {
    let h = harness().await;
    let digest = Digest::of(b"what the page promised");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            detail_json(digest.as_str(), &file_url).into_bytes(),
            "application/vnd.pypi.simple.v1+json",
        ))
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/flask.whl"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"what upstream delivered".to_vec()))
        .mount(&h.server)
        .await;
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    let outcomes = futures_util::future::join_all([
        live_stream_for(&h.state.serving, &digest),
        live_stream_for(&h.state.serving, &digest),
    ])
    .await;
    for outcome in outcomes {
        let mut stream = live_stream(outcome).expect("a live stream");
        let mut saw_error = false;
        while let Some(item) = stream.next().await {
            saw_error |= item.is_err();
        }
        assert!(saw_error);
    }
    assert!(h.state.serving.blobs.head(&digest).await.unwrap().is_none());
}

#[tokio::test]
async fn test_abandoned_download_still_fills_the_cache() {
    let h = harness().await;
    let body = vec![0xDDu8; 16 * 1024];
    let digest = Digest::of(&body);
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    Mock::given(method("GET"))
        .and(path("/files/flask.whl"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .mount(&h.server)
        .await;
    h.state
        .serving
        .meta
        .put_file_url(digest.as_str(), &file_url, "pypi")
        .unwrap();
    let outcome = live_stream_for(&h.state.serving, &digest).await;
    assert!(matches!(outcome, cache::FileOutcome::Live(_)));
    let mut handle = h.state.serving.downloads.get(digest.as_str()).unwrap();
    drop(outcome);
    tokio::time::timeout(std::time::Duration::from_secs(2), transfer_result(&mut handle))
        .await
        .expect("the detached transfer completes")
        .unwrap();
    assert!(h.state.serving.blobs.head(&digest).await.unwrap().is_some());
}

#[tokio::test]
async fn test_stage_cleanup_error_removes_the_live_download() {
    let h = harness().await;
    let digest = Digest::of(b"complete body");
    let upstream = truncated_upstream();
    h.state
        .serving
        .meta
        .put_file_url(digest.as_str(), &upstream.url, "pypi")
        .unwrap();
    let outcome = live_stream_for(&h.state.serving, &digest).await;
    let mut handle = h.state.serving.downloads.get(digest.as_str()).unwrap();
    let stage = std::fs::read_dir(h.dir.path().join("blobs"))
        .unwrap()
        .find(|entry| entry.as_ref().is_ok_and(|entry| entry.file_type().unwrap().is_file()))
        .unwrap()
        .unwrap()
        .path();
    std::fs::remove_file(&stage).unwrap();
    std::fs::create_dir(&stage).unwrap();
    upstream.release();
    drop(outcome);
    let error = tokio::time::timeout(std::time::Duration::from_secs(5), transfer_result(&mut handle))
        .await
        .expect("the failed transfer completes")
        .unwrap_err();
    assert!(!error.is_empty());
    assert!(h.state.serving.downloads.get(digest.as_str()).is_none());
    assert!(h.state.serving.blobs.head(&digest).await.unwrap().is_none());
}

struct RemoteAvailability {
    blobs: BlobStorage,
    content: Bytes,
}

#[async_trait::async_trait]
impl peryx_ha::BlobAvailability for RemoteAvailability {
    async fn ensure_local(
        &self,
        digest: &Digest,
    ) -> Result<Option<peryx_storage::blob::BlobMetadata>, peryx_ha::BlobAvailabilityError> {
        self.blobs
            .put_bytes_as(&self.content, digest)
            .await
            .map_err(storage_availability_error)?;
        self.blobs.head(digest).await.map_err(storage_availability_error)
    }
}

fn install_remote_availability(state: &mut Arc<AppState>, content: Bytes) {
    let blobs = state.serving.blobs.clone();
    Arc::get_mut(state)
        .unwrap()
        .install_distributed_availability(peryx_ha::AvailabilityStateInstall {
            role: peryx_core::NodeRole::Writer,
            topology: peryx_core::TopologyConfig::default(),
            blobs: peryx_ha::BlobServices::new(
                Some(Arc::new(RemoteAvailability { blobs, content })),
                Arc::new(LocalDurability),
            ),
            analytics: Arc::new(UnavailableCompleteness),
            capabilities: peryx_ha::AvailabilityCapabilities::default(),
            authority_drainer: None,
        })
        .expect("remote availability is installed");
}

fn storage_availability_error(error: BlobError) -> peryx_ha::BlobAvailabilityError {
    peryx_ha::BlobAvailabilityError::new(peryx_ha::BlobAvailabilityFailure::Storage, error)
}

#[test]
fn test_remote_availability_classifies_storage_errors() {
    let error = storage_availability_error(BlobError::io(std::io::Error::other("disk unavailable")));
    let source = error.source().expect("blob error source");
    assert_eq!(
        (
            error.kind(),
            error.to_string(),
            source.to_string(),
            source.source().expect("I/O source").to_string(),
        ),
        (
            peryx_ha::BlobAvailabilityFailure::Storage,
            "Storage: I/O error".to_owned(),
            "I/O error".to_owned(),
            "disk unavailable".to_owned(),
        )
    );
}

#[tokio::test]
async fn test_stream_file_serves_a_remote_placement_without_upstream() {
    let mut h = harness().await;
    let content = Bytes::from_static(b"streamed from a verified peer placement");
    let digest = Digest::of(&content);
    install_remote_availability(&mut h.state, content);

    let outcome = live_stream_for(&h.state.serving, &digest).await;

    assert!(matches!(outcome, cache::FileOutcome::Cached(_)));
    assert!(h.state.serving.blobs.head(&digest).await.unwrap().is_some());
}

#[tokio::test]
async fn test_file_path_serves_a_remote_placement_without_upstream() {
    let mut h = harness().await;
    let content = Bytes::from_static(b"materialized from a verified peer placement");
    let digest = Digest::of(&content);
    install_remote_availability(&mut h.state, content);

    let lease = cache::file_path(
        h.state.serving.clone(),
        digest.clone(),
        "pypi".to_owned(),
        "x.whl".to_owned(),
    )
    .await
    .unwrap();

    assert!(lease.path().exists());
    assert!(h.state.serving.blobs.head(&digest).await.unwrap().is_some());
}
