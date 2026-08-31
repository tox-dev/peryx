use std::num::NonZeroUsize;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use bytes::Bytes;
use futures_util::StreamExt as _;
use http_body_util::BodyExt as _;
use peryx_driver::BlockingScanExecutor;
use peryx_storage::blob::{BlobMetadata, BlobRead, BlobReadBody, BlobStorage, Digest};
use peryx_storage::meta::MetaStore;
use tokio::sync::Semaphore;
use tower::ServiceExt as _;

use crate::protocol::Change;
use crate::replica::Replica;
use crate::support::TestServer;
use crate::{
    BlobReference, ChangePage, DEFAULT_MAX_CHANGE_PAGE_SIZE, DEFAULT_MAX_CONCURRENT_CHANGE_PAGES, HttpPrimary,
    HttpPrimaryError, MetadataMutation, PROTOCOL_VERSION, Primary, PrimaryHttpConfigError, follower_router,
    primary_router, primary_router_with_limits,
};

const TOKEN: &str = "replica-secret";

fn seed_applied(meta: &MetaStore, source: &str, count: u64) {
    let changes = (1..=count)
        .map(|serial| Change {
            serial,
            event: format!("event-{serial}").into_bytes(),
            metadata: Vec::new(),
            blobs: Vec::new(),
        })
        .collect();
    let page = ChangePage {
        version: PROTOCOL_VERSION,
        source: source.to_owned(),
        after: 0,
        current_serial: count,
        changes,
    };
    Replica::new(meta, NonZeroUsize::new(10).unwrap())
        .apply_page(page)
        .unwrap();
}

#[tokio::test]
async fn test_follower_serves_the_authoritative_source_up_to_its_applied_serial() {
    let stores = TestStores::new();
    seed_applied(&stores.meta, "writer-a", 3);

    let response = follower_router(TOKEN, stores.meta.clone())
        .unwrap()
        .oneshot(authenticated_request(
            "/+replication/v1/changes?after=0&limit=10",
            TOKEN,
        ))
        .await
        .unwrap();
    let status = response.status();
    let page = serde_json::from_slice::<ChangePage>(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(page.source, "writer-a");
    assert_eq!(page.current_serial, 3);
    assert_eq!(
        page.changes.iter().map(|change| change.serial).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
}

#[tokio::test]
async fn test_follower_preserves_state_at_each_replicated_serial() {
    let middle = TestStores::new();
    let downstream = TestStores::new();
    let changes = vec![artifact_change(1, b"first"), artifact_change(2, b"second")];
    Replica::new(&middle.meta, NonZeroUsize::new(2).unwrap())
        .apply_page(ChangePage {
            version: PROTOCOL_VERSION,
            source: "writer-a".to_owned(),
            after: 0,
            current_serial: 2,
            changes: changes.clone(),
        })
        .unwrap();
    let downstream_replica = Replica::new(&downstream.meta, NonZeroUsize::new(1).unwrap());

    let first_page = follower_page(&middle.meta, 0, 1).await;
    downstream_replica.apply_page(first_page.clone()).unwrap();

    assert_eq!(
        (
            first_page.changes,
            downstream.meta.get_driver_value("artifact").unwrap()
        ),
        (vec![changes[0].clone()], Some(b"first".to_vec()))
    );

    let second_page = follower_page(&middle.meta, 1, 1).await;
    downstream_replica.apply_page(second_page.clone()).unwrap();

    assert_eq!(
        (
            second_page.changes,
            downstream.meta.get_driver_value("artifact").unwrap()
        ),
        (vec![changes[1].clone()], Some(b"second".to_vec()))
    );
}

fn artifact_change(serial: u64, value: &[u8]) -> Change {
    Change {
        serial,
        event: format!("event-{serial}").into_bytes(),
        metadata: vec![MetadataMutation::Put {
            key: "artifact".to_owned(),
            value: value.to_vec(),
        }],
        blobs: vec![BlobReference {
            sha256: Digest::of(value).as_str().to_owned(),
            size: value.len() as u64,
        }],
    }
}

async fn follower_page(meta: &MetaStore, after: u64, limit: usize) -> ChangePage {
    let response = follower_router(TOKEN, meta.clone())
        .unwrap()
        .oneshot(authenticated_request(
            &format!("/+replication/v1/changes?after={after}&limit={limit}"),
            TOKEN,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

#[tokio::test]
async fn test_follower_reports_unavailable_before_it_syncs_a_source() {
    let stores = TestStores::new();

    let response = follower_router(TOKEN, stores.meta.clone())
        .unwrap()
        .oneshot(authenticated_request(
            "/+replication/v1/changes?after=0&limit=10",
            TOKEN,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn test_follower_requires_its_bearer_token() {
    let stores = TestStores::new();
    seed_applied(&stores.meta, "writer-a", 1);

    let response = follower_router(TOKEN, stores.meta.clone())
        .unwrap()
        .oneshot(authenticated_request(
            "/+replication/v1/changes?after=0&limit=10",
            "wrong",
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_follower_rejects_a_bad_limit() {
    let stores = TestStores::new();
    seed_applied(&stores.meta, "writer-a", 1);

    let response = follower_router(TOKEN, stores.meta.clone())
        .unwrap()
        .oneshot(authenticated_request("/+replication/v1/changes?after=0&limit=0", TOKEN))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[test]
fn test_follower_router_rejects_an_empty_token() {
    let stores = TestStores::new();
    assert_eq!(
        follower_router("", stores.meta).unwrap_err(),
        PrimaryHttpConfigError::EmptyToken
    );
}

#[tokio::test]
async fn test_follower_reports_a_replica_state_read_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("empty.redb");
    drop(redb::Database::create(&path).unwrap());

    let response = follower_router(TOKEN, MetaStore::open_existing(path).unwrap())
        .unwrap()
        .oneshot(authenticated_request(
            "/+replication/v1/changes?after=0&limit=10",
            TOKEN,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

fn a_permit() -> tokio::sync::OwnedSemaphorePermit {
    Arc::new(Semaphore::new(1)).try_acquire_owned().unwrap()
}

#[tokio::test]
async fn test_blob_body_preserves_a_backend_stream() {
    let digest = Digest::of(b"artifact");
    let read = BlobRead::new(
        "stream",
        digest,
        BlobMetadata {
            bytes: 8,
            modified: None,
        },
        0..8,
        BlobReadBody::Stream(futures_util::stream::once(async { Ok(Bytes::from_static(b"artifact")) }).boxed()),
    );

    let body = crate::http::blob_body(read, a_permit())
        .collect()
        .await
        .unwrap()
        .to_bytes();

    assert_eq!(body, b"artifact".as_slice());
}

struct TestStores {
    _dir: tempfile::TempDir,
    meta: MetaStore,
    blobs: BlobStorage,
}

impl TestStores {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        Self {
            meta: MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
            blobs: BlobStorage::filesystem(dir.path().join("blobs")),
            _dir: dir,
        }
    }

    fn router(&self) -> Router {
        primary_router("primary-a", TOKEN, self.meta.clone(), self.blobs.clone()).unwrap()
    }

    fn router_with_limit(&self, streams: usize) -> Router {
        primary_router_with_limits(
            "primary-a",
            TOKEN,
            self.meta.clone(),
            self.blobs.clone(),
            NonZeroUsize::new(streams).unwrap(),
            BlockingScanExecutor::new(DEFAULT_MAX_CONCURRENT_CHANGE_PAGES),
        )
        .unwrap()
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
        "Bearer realm=\"peryx-ha-distributed\""
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
        BlobStorage::filesystem(dir.path().join("blobs")),
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
    let digest = stores.blobs.put_bytes(b"artifact bytes").await.unwrap();

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
    let content_length = response.headers()[header::CONTENT_LENGTH].clone();
    let etag = response.headers()[header::ETAG].clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type, "application/octet-stream");
    assert_eq!(content_length, "14");
    assert_eq!(etag, format!("\"sha256:{}\"", digest.as_str()));
    assert_eq!(body, "artifact bytes");
}

#[tokio::test]
async fn test_primary_router_proves_blob_availability_without_a_body() {
    let stores = TestStores::new();
    let digest = stores.blobs.put_bytes(b"artifact bytes").await.unwrap();
    let response = stores
        .router()
        .oneshot(
            Request::head(format!("/+replication/v1/blobs/sha256/{}", digest.as_str()))
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let content_length = response.headers()[header::CONTENT_LENGTH].clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_length, "14");
    assert!(body.is_empty());
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
    assert_eq!(response.headers()["x-peryx-blob-result"], "not-found");
}

#[cfg(unix)]
#[tokio::test]
async fn test_primary_router_reports_an_unreadable_blob() {
    use std::os::unix::fs::PermissionsExt as _;

    let stores = TestStores::new();
    let digest = stores.blobs.put_bytes(b"unreadable").await.unwrap();
    let lease = stores.blobs.materialize(&digest).await.unwrap();
    std::fs::set_permissions(lease.path(), std::fs::Permissions::from_mode(0o000)).unwrap();

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
async fn test_primary_router_serves_a_byte_range() {
    let stores = TestStores::new();
    let digest = stores.blobs.put_bytes(b"0123456789").await.unwrap();

    let response = stores
        .router()
        .oneshot(range_request(&digest, "bytes=2-5"))
        .await
        .unwrap();
    let status = response.status();
    let content_range = response.headers()[header::CONTENT_RANGE].clone();
    let content_length = response.headers()[header::CONTENT_LENGTH].clone();
    let accept_ranges = response.headers()[header::ACCEPT_RANGES].clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();

    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(content_range, "bytes 2-5/10");
    assert_eq!(content_length, "4");
    assert_eq!(accept_ranges, "bytes");
    assert_eq!(body, "2345");
}

#[tokio::test]
async fn test_primary_router_rejects_an_unsatisfiable_range() {
    let stores = TestStores::new();
    let digest = stores.blobs.put_bytes(b"0123456789").await.unwrap();

    let response = stores
        .router()
        .oneshot(range_request(&digest, "bytes=20-30"))
        .await
        .unwrap();
    let status = response.status();
    let content_range = response.headers()[header::CONTENT_RANGE].clone();

    assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(content_range, "bytes */10");
}

#[tokio::test]
async fn test_primary_router_ignores_a_malformed_range_and_serves_the_whole_blob() {
    let stores = TestStores::new();
    let digest = stores.blobs.put_bytes(b"0123456789").await.unwrap();

    let response = stores
        .router()
        .oneshot(range_request(&digest, "bytes=not-a-range"))
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "0123456789");
}

#[tokio::test]
async fn test_primary_router_reports_a_missing_blob_for_a_range() {
    let stores = TestStores::new();
    let digest = Digest::of(b"absent");

    let response = stores
        .router()
        .oneshot(range_request(&digest, "bytes=0-1"))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[cfg(unix)]
#[tokio::test]
async fn test_primary_router_reports_a_range_head_failure() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().unwrap();
    let blobs_dir = dir.path().join("blobs");
    let blobs = BlobStorage::filesystem(blobs_dir.clone());
    let digest = blobs.put_bytes(b"payload").await.unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let router = primary_router("primary-a", TOKEN, meta, blobs).unwrap();
    std::fs::set_permissions(&blobs_dir, std::fs::Permissions::from_mode(0o000)).unwrap();

    let status = router
        .oneshot(range_request(&digest, "bytes=0-3"))
        .await
        .unwrap()
        .status();

    std::fs::set_permissions(&blobs_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn test_primary_router_refuses_a_stream_when_at_capacity() {
    let stores = TestStores::new();
    let digest = stores.blobs.put_bytes(b"artifact bytes").await.unwrap();
    let router = stores.router_with_limit(1);
    let path = format!("/+replication/v1/blobs/sha256/{}", digest.as_str());

    let holding = router
        .clone()
        .oneshot(authenticated_request(&path, TOKEN))
        .await
        .unwrap();
    assert_eq!(holding.status(), StatusCode::OK);

    let refused = router
        .clone()
        .oneshot(authenticated_request(&path, TOKEN))
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(refused.headers()[header::RETRY_AFTER], "1");

    drop(holding);
    let reopened = router.oneshot(authenticated_request(&path, TOKEN)).await.unwrap();
    assert_eq!(reopened.status(), StatusCode::OK);
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
async fn test_http_primary_fetches_changes() {
    let stores = TestStores::new();
    stores.meta.put_driver_value("delete", b"old").unwrap();
    let digest = stores.blobs.put_bytes(b"artifact").await.unwrap();
    stores
        .meta
        .commit_driver_txn(|txn| {
            txn.remove("delete")?;
            txn.put("alpha\0upload", b"record")?;
            txn.reference_blob(digest.as_str(), 8);
            Ok::<_, peryx_storage::meta::MetaError>(((), vec![b"event".to_vec()]))
        })
        .unwrap();
    let server = TestServer::start(Router::new().nest("/mirror", stores.router())).await;
    let primary = HttpPrimary::new(&format!("{}mirror", server.url), TOKEN).unwrap();

    let page = primary.changes(0, 10).await.unwrap();

    assert_eq!(page.current_serial, 1);
    assert_eq!(page.changes[0].event, b"event");
    assert_eq!(
        page.changes[0].blobs,
        vec![crate::BlobReference {
            sha256: digest.as_str().to_owned(),
            size: 8,
        }]
    );
    assert_eq!(
        page.changes[0].metadata,
        vec![
            crate::MetadataMutation::Put {
                key: "alpha\0upload".to_owned(),
                value: b"record".to_vec(),
            },
            crate::MetadataMutation::Delete {
                key: "delete".to_owned(),
            },
        ]
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

#[tokio::test]
async fn test_http_primary_rejects_an_oversized_declared_length() {
    let server = TestServer::start(Router::new().route(
        "/+replication/v1/changes",
        axum::routing::get(|| async { " ".repeat(128) }),
    ))
    .await;
    let primary = HttpPrimary::build(&server.url, TOKEN, 64).unwrap();

    let result = primary.changes(0, 10).await;

    assert!(matches!(
        result,
        Err(HttpPrimaryError::ResponseTooLarge { limit: 64, actual: 128 })
    ));
}

#[tokio::test]
async fn test_http_primary_stops_a_chunked_body_that_crosses_the_limit() {
    let server = TestServer::start(Router::new().route(
        "/+replication/v1/changes",
        axum::routing::get(|| async {
            let chunks =
                futures_util::stream::iter((0..4).map(|_| Ok::<_, std::io::Error>(Bytes::from_static(&[b' '; 32]))));
            Body::from_stream(chunks)
        }),
    ))
    .await;
    let primary = HttpPrimary::build(&server.url, TOKEN, 64).unwrap();

    let result = primary.changes(0, 10).await;

    assert!(
        matches!(result, Err(HttpPrimaryError::ResponseTooLarge { limit: 64, actual }) if actual > 64),
        "the read stops only once the accumulated bytes cross the cap"
    );
}

#[tokio::test]
async fn test_http_primary_accepts_a_page_at_the_byte_limit() {
    let page = ChangePage {
        version: PROTOCOL_VERSION,
        source: "primary-a".to_owned(),
        after: 0,
        current_serial: 0,
        changes: Vec::new(),
    };
    let encoded = serde_json::to_vec(&page).unwrap();
    let limit = encoded.len() as u64;
    let server = TestServer::start(Router::new().route(
        "/+replication/v1/changes",
        axum::routing::get(move || {
            let encoded = encoded.clone();
            async move { encoded }
        }),
    ))
    .await;
    let primary = HttpPrimary::build(&server.url, TOKEN, limit).unwrap();

    let fetched = primary.changes(0, 10).await.unwrap();

    assert_eq!(fetched, page);
}

fn authenticated_request(uri: &str, token: &str) -> Request<Body> {
    Request::get(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

fn range_request(digest: &Digest, range: &str) -> Request<Body> {
    Request::get(format!("/+replication/v1/blobs/sha256/{}", digest.as_str()))
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header(header::RANGE, range)
        .body(Body::empty())
        .unwrap()
}
