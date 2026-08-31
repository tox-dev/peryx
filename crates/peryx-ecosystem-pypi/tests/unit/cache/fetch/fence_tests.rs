use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use peryx_driver::AppState;
use peryx_identity::IndexAcl;
use peryx_index::{Index, IndexKind};
use peryx_policy::Policy;
use peryx_storage::blob::BlobStorage;
use peryx_storage::meta::MetaStore;
use peryx_upstream::UpstreamClient;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, mpsc};

use super::{RefreshSummary, refresh_stale_pages};
use crate::cache::materialize_detail;
use crate::store::{CachedIndex, PypiStore as _};

const KEY: &str = "pypi/flask";
const SIMPLE_JSON: &str = "application/vnd.pypi.simple.v1+json";
const SEEDED_PAGE: &str = r#"{"meta":{"api-version":"1.0"},"name":"flask","files":[]}"#;
const UPSTREAM_PAGE: &str = concat!(
    r#"{"meta":{"api-version":"1.0"},"name":"flask","files":[{"filename":"flask-1.0.tar.gz","#,
    r#""url":"https://files.example/flask-1.0.tar.gz","hashes":{"sha256":"#,
    r#""1111111111111111111111111111111111111111111111111111111111111111"}}]}"#,
);

struct Fixture {
    _dir: tempfile::TempDir,
    state: Arc<AppState>,
}

/// A cached `pypi` index pointed at `base`, holding one project page stale enough that every writer
/// must revalidate it: `fetched_at_unix` of zero is past both the TTL and the stale-serving bound.
fn fixture(base: &str) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let mut state = AppState::new(
        MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        BlobStorage::filesystem(dir.path().join("blobs")),
        60,
        vec![Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Cached {
                client: UpstreamClient::new(base).unwrap(),
                offline: false,
            },
            policy: Policy::default(),
            acl: IndexAcl::default(),
        }],
    );
    crate::tests::install(&mut state);
    state
        .serving
        .meta
        .put_index(
            KEY,
            &CachedIndex {
                etag: None,
                last_serial: None,
                fetched_at_unix: 0,
                content_type: Some(SIMPLE_JSON.to_owned()),
                fresh_secs: None,
                body: SEEDED_PAGE.as_bytes().to_vec(),
            },
        )
        .unwrap();
    Fixture {
        _dir: dir,
        state: Arc::new(state),
    }
}

#[derive(Clone)]
struct Upstream {
    arrivals: mpsc::Sender<()>,
    release: Arc<Semaphore>,
    requests: Arc<AtomicUsize>,
}

/// Answer one project-page request, but only once the test has released it: parking every fetch in
/// the handler is what lets a test hold one writer inside upstream while a second writer arrives.
async fn hold_page(mut connection: TcpStream, upstream: Upstream) {
    let mut request = Vec::new();
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        let mut chunk = [0; 1024];
        let read = connection.read(&mut chunk).await.unwrap();
        assert_ne!(read, 0, "the request ended before its headers");
        request.extend_from_slice(&chunk[..read]);
    }
    upstream.requests.fetch_add(1, Ordering::SeqCst);
    upstream.arrivals.send(()).await.unwrap();
    upstream.release.acquire().await.unwrap().forget();
    let head = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: {SIMPLE_JSON}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        UPSTREAM_PAGE.len()
    );
    connection.write_all(head.as_bytes()).await.unwrap();
    connection.write_all(UPSTREAM_PAGE.as_bytes()).await.unwrap();
}

/// Serves `listener` for as long as `run` needs it, so the fixture leaves no accept loop behind.
async fn serve_until_done<T, H, F>(listener: TcpListener, run: impl Future<Output = T>, handle: H) -> T
where
    H: Fn(TcpStream) -> F,
    F: Future<Output = ()> + Send + 'static,
{
    let mut run = Box::pin(run);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                tokio::spawn(handle(accepted.unwrap().0));
            }
            outcome = &mut run => return outcome,
        }
    }
}

/// The flight a second writer for `KEY` joins, once the writer ahead of it holds one.
///
/// Subscribing only succeeds while a gate for the key exists, so this is also the assertion that the
/// first writer took the flight at all rather than fetching outside it.
fn joins(state: &AppState) -> peryx_index::serving::FlightEvents {
    state
        .serving
        .cache
        .inflight
        .subscribe(KEY)
        .expect("a page revalidation holds its project flight")
}

fn spawn_sweep(state: &Arc<AppState>) -> tokio::task::JoinHandle<RefreshSummary> {
    let serving = state.serving.clone();
    tokio::spawn(async move { refresh_stale_pages(&serving).await.unwrap() })
}

fn stored_page(state: &AppState) -> String {
    let record = state.serving.meta.get_index(KEY).unwrap().unwrap();
    String::from_utf8(record.body).unwrap()
}

#[tokio::test]
async fn test_a_queued_sweep_keeps_the_page_the_flight_winner_published() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/simple/", listener.local_addr().unwrap());
    let fixture = fixture(&base);
    let (arrivals, mut arrived) = mpsc::channel(4);
    let release = Arc::new(Semaphore::new(0));
    let requests = Arc::new(AtomicUsize::new(0));
    let upstream = Upstream {
        arrivals,
        release: release.clone(),
        requests: requests.clone(),
    };
    let state = fixture.state.clone();

    let (winner, queued) = serve_until_done(
        listener,
        async move {
            let winner = spawn_sweep(&state);
            arrived.recv().await.unwrap();
            let mut joined = joins(&state);
            let queued = spawn_sweep(&state);
            joined.next_join().await.unwrap();
            release.add_permits(2);
            (winner.await.unwrap(), queued.await.unwrap())
        },
        move |connection| hold_page(connection, upstream.clone()),
    )
    .await;

    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert_eq!(winner, RefreshSummary { checked: 1, changed: 1 });
    assert_eq!(queued, RefreshSummary::default());
    assert!(stored_page(&fixture.state).contains("flask-1.0.tar.gz"));
}

#[tokio::test]
async fn test_a_live_refresh_serves_the_page_the_sweep_published() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/simple/", listener.local_addr().unwrap());
    let fixture = fixture(&base);
    let (arrivals, mut arrived) = mpsc::channel(4);
    let release = Arc::new(Semaphore::new(0));
    let requests = Arc::new(AtomicUsize::new(0));
    let upstream = Upstream {
        arrivals,
        release: release.clone(),
        requests: requests.clone(),
    };
    let state = fixture.state.clone();

    let (swept, served) = serve_until_done(
        listener,
        async move {
            let sweep = spawn_sweep(&state);
            arrived.recv().await.unwrap();
            let mut joined = joins(&state);
            let live = tokio::spawn(materialize_detail(state.serving.clone(), 0, "flask".to_owned()));
            joined.next_join().await.unwrap();
            release.add_permits(2);
            (sweep.await.unwrap(), live.await.unwrap().unwrap())
        },
        move |connection| hold_page(connection, upstream.clone()),
    )
    .await;

    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert_eq!(swept, RefreshSummary { checked: 1, changed: 1 });
    let filenames = served
        .expect("the project exists upstream")
        .files
        .into_iter()
        .map(|file| file.filename)
        .collect::<Vec<_>>();
    assert_eq!(filenames, vec!["flask-1.0.tar.gz".to_owned()]);
    assert!(stored_page(&fixture.state).contains("flask-1.0.tar.gz"));
}
