//! The harness the streaming and serving `PyPI` tests build on.

pub(super) use std::sync::Arc;
pub(super) use std::sync::atomic::Ordering;

pub(super) use crate::SimpleError;
pub(super) use crate::store::CachedIndex;
pub(super) use crate::store::PypiStore as _;
pub(super) use axum::http::StatusCode;
pub(super) use bytes::Bytes;
pub(super) use futures_util::StreamExt as _;
pub(super) use peryx_storage::blob::{BlobError, BlobStore, Digest};
pub(super) use peryx_storage::meta::{MetaError, MetaStore};
pub(super) use peryx_upstream::UpstreamClient;
pub(super) use wiremock::matchers::{method, path};
pub(super) use wiremock::{Mock, MockServer, ResponseTemplate};

pub(super) use crate::cache::{self, PageOutcome};
pub(super) use crate::tests::http::{detail_json, get, harness};
pub(super) use peryx_driver::state::AppState;
pub(super) use peryx_index::{Index, IndexKind};
pub(super) use peryx_policy::{Policy, PolicyAction, PolicyConfig};

pub(super) fn fresh_record(body: &[u8]) -> CachedIndex {
    CachedIndex {
        etag: None,
        last_serial: None,
        fetched_at_unix: 1000,
        content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
        fresh_secs: None,
        body: body.to_vec(),
    }
}

pub(super) async fn mount_json_page(server: &MockServer, body: &str) {
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_vec(), "application/vnd.pypi.simple.v1+json"),
        )
        .mount(server)
        .await;
}

pub(super) fn split_project_upstream(first: Vec<u8>, rest: Vec<u8>) -> (String, std::sync::mpsc::Sender<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (release, released) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        use std::io::{Read as _, Write as _};
        let (mut socket, _) = listener.accept().unwrap();
        let mut buffer = [0u8; 1024];
        let _ = socket.read(&mut buffer);
        let header = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/vnd.pypi.simple.v1+json\r\ncontent-length: {}\r\n\r\n",
            first.len() + rest.len()
        );
        socket.write_all(header.as_bytes()).unwrap();
        socket.write_all(&first).unwrap();
        socket.flush().unwrap();
        released.recv().unwrap();
        socket.write_all(&rest).unwrap();
    });
    (format!("http://{addr}/simple/"), release)
}

/// A state with the given indexes over a fresh store, for topologies the shared harness lacks.
pub(super) fn custom_state(
    dir: &tempfile::TempDir,
    upstream: &str,
    indexes: fn(UpstreamClient) -> Vec<Index>,
) -> Arc<AppState> {
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let client = UpstreamClient::new(upstream).unwrap();
    crate::tests::wired(AppState::with_clock(
        meta,
        blobs,
        60,
        indexes(client),
        Arc::new(|| 1000),
    ))
}

pub(super) async fn stream_outcome(state: &Arc<AppState>) -> Vec<Result<Bytes, std::io::Error>> {
    match cache::stream_detail(state.serving.clone(), 0, "flask".to_owned())
        .await
        .unwrap()
    {
        PageOutcome::Streaming(stream, _) => stream.collect().await,
        outcome => panic!("expected a streaming outcome, got {}", matches_name(&outcome)),
    }
}

pub(super) fn matches_name(outcome: &PageOutcome) -> &'static str {
    match outcome {
        PageOutcome::Ready(_, _) => "Ready",
        PageOutcome::Streaming(_, _) => "Streaming",
        PageOutcome::NotFound => "NotFound",
        PageOutcome::Fallback => "Fallback",
    }
}
