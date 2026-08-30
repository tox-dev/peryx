use std::sync::Arc;

use crate::store::PypiStore as _;
use futures_util::TryStreamExt as _;
use peryx_driver::state::{AppState, ServingState};
use peryx_index::{Index, IndexKind};
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;
use peryx_upstream::UpstreamClient;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

type PageByteStream = futures_util::stream::BoxStream<'static, Result<Bytes, std::io::Error>>;
type StreamingParts = Result<(PageByteStream, Option<u64>), PageOutcome>;

#[tokio::test]
async fn test_complete_preflight_returns_a_persistence_error() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    mount_page(&server, r#"{"name":"flask"}"#).await;
    let state = read_only_state(&dir, &server);

    assert!(matches!(
        crate::cache::stream_detail(state.clone(), 0, "flask".to_owned()).await,
        Err(CacheError::Meta(_))
    ));
    assert!(state.meta.get_index("pypi/flask").unwrap().is_none());
}

#[tokio::test]
async fn test_live_stream_survives_a_persistence_error() {
    crate::tests::install_global_subscriber();
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let page = r#"{"meta":{"api-version":"1.4"},"project-status":{},"name":"flask","versions":["1.0"],"files":[]}"#;
    mount_page(&server, page).await;
    let state = read_only_state(&dir, &server);
    let representation_key = state.representation_key("pypi", "flask", crate::cache::SIMPLE_JSON);
    let outcome = crate::cache::stream_detail(state.clone(), 0, "flask".to_owned())
        .await
        .unwrap();
    let (stream, serial) = streaming_parts(outcome).ok().unwrap();
    assert_eq!(
        (
            stream.try_collect::<Vec<_>>().await.unwrap().concat(),
            serial,
            state.meta.get_index("pypi/flask").unwrap(),
            state.cache.hot_fresh_versioned(&representation_key, 1000),
        ),
        (
            page.as_bytes().to_vec(),
            None,
            None,
            Some((Bytes::copy_from_slice(page.as_bytes()), None)),
        ),
    );
}

#[tokio::test]
async fn test_complete_preflight_returns_a_ready_page() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let page = r#"{"name":"flask"}"#;
    mount_page(&server, page).await;
    let state = writable_state(&dir, &server);

    let outcome = crate::cache::stream_detail(state, 0, "flask".to_owned()).await.unwrap();

    assert!(matches!(
        streaming_parts(outcome),
        Err(PageOutcome::Ready(bytes, None)) if bytes == page
    ));
}

async fn mount_page(server: &MockServer, body: &str) {
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_vec(), "application/vnd.pypi.simple.v1+json"),
        )
        .mount(server)
        .await;
}

fn read_only_state(dir: &tempfile::TempDir, server: &MockServer) -> Arc<ServingState> {
    let database = dir.path().join("peryx.redb");
    drop(MetaStore::open(&database).unwrap());
    state(dir, server, MetaStore::open_existing_read_only(database).unwrap())
}

fn writable_state(dir: &tempfile::TempDir, server: &MockServer) -> Arc<ServingState> {
    state(dir, server, MetaStore::open(dir.path().join("peryx.redb")).unwrap())
}

fn state(dir: &tempfile::TempDir, server: &MockServer, meta: MetaStore) -> Arc<ServingState> {
    let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let mut app = AppState::with_clock(
        meta,
        BlobStore::new(dir.path().join("blobs")),
        60,
        vec![Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Cached { client, offline: false },
            policy: peryx_policy::Policy::default(),
            acl: peryx_identity::IndexAcl::default(),
        }],
        Arc::new(|| 1000),
    );
    crate::tests::install(&mut app);
    app.serving
}

fn streaming_parts(outcome: PageOutcome) -> StreamingParts {
    match outcome {
        PageOutcome::Streaming(stream, serial) => Ok((stream, serial)),
        outcome => Err(outcome),
    }
}
