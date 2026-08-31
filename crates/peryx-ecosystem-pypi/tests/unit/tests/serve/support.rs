pub(super) use std::sync::Arc;
pub(super) use std::sync::atomic::Ordering;

pub(super) use crate::SimpleError;
pub(super) use crate::store::CachedIndex;
pub(super) use crate::store::PypiStore as _;
pub(super) use axum::http::StatusCode;
pub(super) use bytes::Bytes;
pub(super) use futures_util::StreamExt as _;
pub(super) use peryx_storage::blob::{BlobError, BlobStorage, Digest};
pub(super) use peryx_storage::meta::{MetaError, MetaStore};
pub(super) use peryx_upstream::{NamedUpstream, UpstreamClient, UpstreamRouter};
pub(super) use wiremock::matchers::{method, path};
pub(super) use wiremock::{Mock, MockServer, ResponseTemplate};

pub(super) use crate::cache::{self, PageOutcome};
pub(super) use crate::tests::http::{Harness, detail_json, get, harness};
pub(super) use peryx_driver::state::AppState;
pub(super) use peryx_index::{Index, IndexKind};
pub(super) use peryx_policy::{Policy, PolicyAction, PolicyConfig};

type PageByteStream = futures_util::stream::BoxStream<'static, Result<Bytes, std::io::Error>>;
type StreamingParts = Result<(PageByteStream, Option<u64>), PageOutcome>;

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

pub(super) fn split_project_upstream(first: Vec<u8>, rest: Vec<u8>) -> StalledResponse {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (entered, entry) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let handle = std::thread::spawn(move || {
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
        let _ = entered.send(());
        released.recv().unwrap();
        socket.write_all(&rest).unwrap();
    });
    StalledResponse {
        upstream: format!("http://{addr}/simple/"),
        entered: Some(entry),
        release,
        address: addr,
        handle: Some(handle),
    }
}

pub(super) struct StalledResponse {
    pub(super) upstream: String,
    entered: Option<tokio::sync::oneshot::Receiver<()>>,
    release: std::sync::mpsc::Sender<()>,
    address: std::net::SocketAddr,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl StalledResponse {
    pub(super) async fn wait_until_entered(&mut self) {
        self.entered.take().unwrap().await.unwrap();
    }

    pub(super) fn release(&self) {
        self.release.send(()).unwrap();
    }
}

impl Drop for StalledResponse {
    fn drop(&mut self) {
        let _ = self.release.send(());
        let _ = std::net::TcpStream::connect(self.address);
        let joined = self.handle.take().unwrap().join();
        if !std::thread::panicking() {
            joined.expect("upstream fixture panicked");
        }
    }
}

pub(super) fn stalled_response(status: u16, body: Vec<u8>) -> StalledResponse {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (entered, entry) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        use std::io::{Read as _, Write as _};
        let (mut socket, _) = listener.accept().unwrap();
        let mut request = [0; 1024];
        let _ = socket.read(&mut request);
        entered.send(()).unwrap();
        released.recv().unwrap();
        let reason = if status == 200 { "OK" } else { "Not Found" };
        write!(
            socket,
            "HTTP/1.1 {status} {reason}\r\ncontent-type: application/vnd.pypi.simple.v1+json\r\ncontent-length: {}\r\n\r\n",
            body.len()
        )
        .unwrap();
        socket.write_all(&body).unwrap();
    });
    StalledResponse {
        upstream: format!("http://{addr}/simple/"),
        entered: Some(entry),
        release,
        address: addr,
        handle: Some(handle),
    }
}

pub(super) fn revalidating_upstream() -> StalledResponse {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (entered, entry) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        use std::io::{Read as _, Write as _};
        let (mut response, _) = listener.accept().unwrap();
        let mut request = [0; 1024];
        let _ = response.read(&mut request);
        entered.send(()).unwrap();
        released.recv().unwrap();
        response
            .write_all(b"HTTP/1.1 304 Not Modified\r\ncontent-length: 0\r\n\r\n")
            .unwrap();
    });
    StalledResponse {
        upstream: format!("http://{addr}/simple/"),
        entered: Some(entry),
        release,
        address: addr,
        handle: Some(handle),
    }
}

pub(super) struct ResponseServer {
    pub(super) upstream: String,
    address: std::net::SocketAddr,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Drop for ResponseServer {
    fn drop(&mut self) {
        let _ = std::net::TcpStream::connect(self.address);
        let joined = self.handle.take().unwrap().join();
        if !std::thread::panicking() {
            joined.expect("upstream fixture panicked");
        }
    }
}

pub(super) fn response_server(response: &'static [u8]) -> ResponseServer {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        use std::io::{Read as _, Write as _};
        if let Ok((mut socket, _)) = listener.accept() {
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request);
            let _ = socket.write_all(response);
        }
    });
    ResponseServer {
        upstream: format!("http://{address}/simple/"),
        address,
        handle: Some(handle),
    }
}

pub(super) fn cached_state(dir: &tempfile::TempDir, upstream: &str) -> Arc<AppState> {
    custom_state(dir, upstream, |client| {
        vec![Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Cached { client, offline: false },
            policy: Policy::default(),
            acl: peryx_identity::IndexAcl::default(),
        }]
    })
}

pub(super) fn custom_state(
    dir: &tempfile::TempDir,
    upstream: &str,
    indexes: fn(UpstreamClient) -> Vec<Index>,
) -> Arc<AppState> {
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let client = UpstreamClient::new(upstream).unwrap();
    crate::tests::wired(AppState::with_clock(
        meta,
        blobs,
        60,
        indexes(client),
        Arc::new(|| 1000),
    ))
}

pub(super) fn routed_state(dir: &tempfile::TempDir, primary: UpstreamClient, router: UpstreamRouter) -> Arc<AppState> {
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let mut state = AppState::with_clock(
        meta,
        blobs,
        60,
        vec![Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Cached {
                client: primary,
                offline: false,
            },
            policy: Policy::default(),
            acl: peryx_identity::IndexAcl::default(),
        }],
        Arc::new(|| 1000),
    );
    Arc::get_mut(&mut state.serving)
        .unwrap()
        .upstream_routes
        .insert("pypi".to_owned(), router);
    crate::tests::wired(state)
}

pub(super) fn streaming_parts(outcome: PageOutcome) -> StreamingParts {
    match outcome {
        PageOutcome::Streaming(stream, serial) => Ok((stream, serial)),
        outcome => Err(outcome),
    }
}
