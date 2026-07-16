use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use bytes::Bytes;
use futures_util::StreamExt as _;
use http_body_util::BodyExt as _;
use peryx_storage::blob::{BlobStore, Digest};
use peryx_storage::meta::MetaStore;
use tower::ServiceExt as _;

use crate::{
    ChangePage, DEFAULT_MAX_CHANGE_PAGE_SIZE, HttpPrimary, HttpPrimaryError, PROTOCOL_VERSION, Primary,
    PrimaryHttpConfigError, primary_router,
};

const TOKEN: &str = "replica-secret";

struct TestStores {
    _dir: tempfile::TempDir,
    meta: MetaStore,
    blobs: BlobStore,
}

impl TestStores {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        Self {
            meta: MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
            blobs: BlobStore::new(dir.path().join("blobs")),
            _dir: dir,
        }
    }

    fn router(&self) -> Router {
        primary_router("primary-a", TOKEN, self.meta.clone(), self.blobs.clone()).unwrap()
    }
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

#[test]
fn test_primary_router_rejects_an_empty_source() {
    let stores = TestStores::new();

    let result = primary_router("", TOKEN, stores.meta, stores.blobs);

    assert_eq!(result.unwrap_err(), PrimaryHttpConfigError::EmptySource);
}

#[test]
fn test_primary_router_rejects_an_empty_token() {
    let stores = TestStores::new();

    let result = primary_router("primary-a", "", stores.meta, stores.blobs);

    assert_eq!(result.unwrap_err(), PrimaryHttpConfigError::EmptyToken);
}

#[tokio::test]
async fn test_primary_router_requires_its_bearer_token() {
    let stores = TestStores::new();
    let response = stores
        .router()
        .oneshot(
            Request::get("/+replication/v1/changes?after=0&limit=1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response.headers()[header::WWW_AUTHENTICATE],
        "Bearer realm=\"peryx-replication\""
    );
}

#[tokio::test]
async fn test_primary_router_rejects_a_different_bearer_token() {
    let stores = TestStores::new();
    let response = stores
        .router()
        .oneshot(authenticated_request(
            "/+replication/v1/changes?after=0&limit=1",
            "different-secret",
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_primary_router_pages_changes_after_an_exclusive_serial() {
    let stores = TestStores::new();
    stores
        .meta
        .commit_driver_txn(|_| {
            Ok::<_, peryx_storage::meta::MetaError>(((), vec![b"one".to_vec(), b"two".to_vec(), b"three".to_vec()]))
        })
        .unwrap();

    let response = stores
        .router()
        .oneshot(authenticated_request("/+replication/v1/changes?after=1&limit=1", TOKEN))
        .await
        .unwrap();
    let status = response.status();
    let page = serde_json::from_slice::<ChangePage>(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(page.version, PROTOCOL_VERSION);
    assert_eq!(page.source, "primary-a");
    assert_eq!(page.after, 1);
    assert_eq!(page.current_serial, 3);
    assert_eq!(page.changes.len(), 1);
    assert_eq!(page.changes[0].serial, 2);
    assert_eq!(page.changes[0].event, b"two");
}

#[tokio::test]
async fn test_primary_router_reports_a_journal_read_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("empty.redb");
    drop(redb::Database::create(&path).unwrap());
    let router = primary_router(
        "primary-a",
        TOKEN,
        MetaStore::open_existing(path).unwrap(),
        BlobStore::new(dir.path().join("blobs")),
    )
    .unwrap();

    let response = router
        .oneshot(authenticated_request("/+replication/v1/changes?after=0&limit=1", TOKEN))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn test_primary_router_rejects_a_zero_page_limit() {
    let stores = TestStores::new();

    let response = stores
        .router()
        .oneshot(authenticated_request("/+replication/v1/changes?after=0&limit=0", TOKEN))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_primary_router_rejects_an_oversized_page_limit() {
    let stores = TestStores::new();

    let response = stores
        .router()
        .oneshot(authenticated_request(
            &format!(
                "/+replication/v1/changes?after=0&limit={}",
                DEFAULT_MAX_CHANGE_PAGE_SIZE + 1
            ),
            TOKEN,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_primary_router_streams_a_digest_addressed_blob() {
    let stores = TestStores::new();
    let digest = stores.blobs.write(b"artifact bytes").unwrap();

    let response = stores
        .router()
        .oneshot(authenticated_request(
            &format!("/+replication/v1/blobs/sha256/{}", digest.as_str()),
            TOKEN,
        ))
        .await
        .unwrap();
    let status = response.status();
    let content_type = response.headers()[header::CONTENT_TYPE].clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type, "application/octet-stream");
    assert_eq!(body, "artifact bytes");
}

#[tokio::test]
async fn test_primary_router_rejects_an_invalid_blob_digest() {
    let stores = TestStores::new();

    let response = stores
        .router()
        .oneshot(authenticated_request("/+replication/v1/blobs/sha256/invalid", TOKEN))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_primary_router_reports_a_missing_blob() {
    let stores = TestStores::new();
    let digest = Digest::of(b"missing");

    let response = stores
        .router()
        .oneshot(authenticated_request(
            &format!("/+replication/v1/blobs/sha256/{}", digest.as_str()),
            TOKEN,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[cfg(unix)]
#[tokio::test]
async fn test_primary_router_reports_an_unreadable_blob() {
    use std::os::unix::fs::PermissionsExt as _;

    let stores = TestStores::new();
    let digest = stores.blobs.write(b"unreadable").unwrap();
    std::fs::set_permissions(stores.blobs.path_for(&digest), std::fs::Permissions::from_mode(0o000)).unwrap();

    let response = stores
        .router()
        .oneshot(authenticated_request(
            &format!("/+replication/v1/blobs/sha256/{}", digest.as_str()),
            TOKEN,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn test_primary_router_protects_blob_requests() {
    let stores = TestStores::new();
    let digest = Digest::of(b"missing");

    let response = stores
        .router()
        .oneshot(
            Request::get(format!("/+replication/v1/blobs/sha256/{}", digest.as_str()))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[test]
fn test_http_primary_rejects_an_empty_token() {
    assert!(matches!(
        HttpPrimary::new("https://primary.example/", ""),
        Err(HttpPrimaryError::EmptyToken)
    ));
}

#[test]
fn test_http_primary_rejects_an_invalid_url() {
    assert!(matches!(
        HttpPrimary::new("file:///tmp/primary", TOKEN),
        Err(HttpPrimaryError::InvalidBase(_))
    ));
}

#[test]
fn test_http_primary_rejects_a_malformed_url() {
    assert!(matches!(
        HttpPrimary::new("://primary", TOKEN),
        Err(HttpPrimaryError::InvalidBase(_))
    ));
}

#[tokio::test]
async fn test_http_primary_fetches_changes_and_streams_blobs() {
    let stores = TestStores::new();
    stores
        .meta
        .commit_driver_txn(|_| Ok::<_, peryx_storage::meta::MetaError>(((), vec![b"event".to_vec()])))
        .unwrap();
    let digest = stores.blobs.write(b"artifact").unwrap();
    let server = TestServer::start(Router::new().nest("/mirror", stores.router())).await;
    let primary = HttpPrimary::new(&format!("{}mirror", server.url), TOKEN).unwrap();

    let page = primary.changes(0, 10).await.unwrap();
    let chunks = primary.blob(&digest).await.unwrap().collect::<Vec<_>>().await;

    assert_eq!(page.current_serial, 1);
    assert_eq!(page.changes[0].event, b"event");
    assert_eq!(
        chunks.into_iter().collect::<Result<Vec<Bytes>, _>>().unwrap(),
        [Bytes::from_static(b"artifact")]
    );
    let debug = format!("{primary:?}");
    assert!(debug.contains("<redacted>"));
    assert!(!debug.contains(TOKEN));
}

#[tokio::test]
async fn test_http_primary_surfaces_an_auth_status() {
    let stores = TestStores::new();
    let server = TestServer::start(stores.router()).await;
    let primary = HttpPrimary::new(&server.url, "wrong").unwrap();

    let result = primary.changes(0, 10).await;

    assert!(matches!(result, Err(HttpPrimaryError::Request(_))));
}

#[tokio::test]
async fn test_http_primary_reports_an_invalid_change_page() {
    let server = TestServer::start(Router::new().route(
        "/+replication/v1/changes",
        axum::routing::get(|| async { "invalid json" }),
    ))
    .await;
    let primary = HttpPrimary::new(&server.url, TOKEN).unwrap();

    let result = primary.changes(0, 10).await;

    assert!(matches!(result, Err(HttpPrimaryError::Decode(_))));
}

fn authenticated_request(uri: &str, token: &str) -> Request<Body> {
    Request::get(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}
