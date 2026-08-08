use std::num::{NonZeroU64, NonZeroUsize};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use peryx_storage::blob::Digest;

use crate::blob::{BlobRequest, BlobTransport, ByteRange};
use crate::blob_http::{HttpBlobError, HttpBlobTransport};
use crate::peer::{TransferLimits, TransportError};

const BLOB_ROUTE: &str = "/+replication/v1/blobs/sha256/{digest}";
const TOKEN: &str = "secret";

fn limits(max_encoded_bytes: u64) -> TransferLimits {
    TransferLimits {
        max_operations: NonZeroUsize::new(256).unwrap(),
        max_encoded_bytes: NonZeroU64::new(max_encoded_bytes).unwrap(),
    }
}

fn whole(digest: &Digest) -> BlobRequest {
    BlobRequest {
        digest: digest.clone(),
        range: None,
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

fn transport(url: &str, cap: u64) -> HttpBlobTransport {
    HttpBlobTransport::new(url, TOKEN, limits(cap), Duration::from_secs(5)).unwrap()
}

/// A blob server that answers every digest with one preset response.
fn serving(response: impl Fn() -> Response + Clone + Send + Sync + 'static) -> Router {
    Router::new().route(
        BLOB_ROUTE,
        get(move || {
            let response = response.clone();
            async move { response() }
        }),
    )
}

fn bytes_response(body: &'static [u8]) -> Router {
    serving(move || (StatusCode::OK, body).into_response())
}

#[test]
fn test_new_rejects_an_empty_token() {
    let error = HttpBlobTransport::new("http://peer/", "", limits(64), Duration::from_secs(1)).unwrap_err();

    assert!(matches!(error, HttpBlobError::EmptyToken));
}

#[test]
fn test_new_rejects_a_non_http_base() {
    for base in ["not a url", "ftp://peer/"] {
        let error = HttpBlobTransport::new(base, TOKEN, limits(64), Duration::from_secs(1)).unwrap_err();
        assert!(matches!(error, HttpBlobError::InvalidBase(_)), "{base}");
    }
}

#[test]
fn test_new_appends_a_missing_trailing_slash_to_the_base_path() {
    // A base path without a trailing slash must gain one before the blob path joins it, or the two fuse
    // into a single segment (…/replication+replication/…) that points nowhere.
    let transport = HttpBlobTransport::new(
        "https://peer.test/replication",
        TOKEN,
        limits(64),
        Duration::from_secs(1),
    )
    .unwrap();

    let rendered = format!("{transport:?}");
    assert!(
        rendered.contains("/replication/+replication/v1/blobs/sha256/"),
        "{rendered}"
    );
}

#[test]
fn test_debug_redacts_the_token() {
    let rendered = format!("{:?}", transport("http://peer/", 64));

    assert!(rendered.contains("<redacted>"), "{rendered}");
    assert!(!rendered.contains(TOKEN), "{rendered}");
}

#[tokio::test]
async fn test_fetch_verifies_and_returns_a_whole_blob() {
    let content = b"the real blob bytes";
    let digest = Digest::of(content);
    let server = TestServer::start(bytes_response(content)).await;

    let fetched = transport(&server.url, 1024).fetch_blob(whole(&digest)).await.unwrap();

    assert_eq!(fetched, content);
}

#[tokio::test]
async fn test_fetch_rejects_a_whole_blob_that_does_not_match_its_digest() {
    let requested = Digest::of(b"the real blob");
    let substituted = b"substituted bytes";
    let server = TestServer::start(bytes_response(substituted)).await;

    let error = transport(&server.url, 1024)
        .fetch_blob(whole(&requested))
        .await
        .unwrap_err();

    assert_eq!(
        error,
        TransportError::DigestMismatch {
            expected: requested.as_str().to_owned(),
            actual: Digest::of(substituted).as_str().to_owned(),
        },
    );
}

#[tokio::test]
async fn test_fetch_returns_a_range_unverified() {
    // The served bytes do not hash to the requested digest, yet a ranged fetch returns them without
    // error: a partial range cannot be checked against the whole-blob digest, so the transport does not
    // try. The caller re-verifies the reassembled whole at commit.
    let requested = Digest::of(b"the whole blob is longer than this");
    let partial = b"a slice of it";
    let server = TestServer::start(bytes_response(partial)).await;

    let fetched = transport(&server.url, 1024)
        .fetch_blob(BlobRequest {
            digest: requested,
            range: Some(ByteRange { offset: 4, length: 13 }),
        })
        .await
        .unwrap();

    assert_eq!(fetched, partial);
}

#[tokio::test]
async fn test_fetch_caps_a_declared_oversized_blob_before_reading_it() {
    let server = TestServer::start(bytes_response(b"0123456789")).await;

    let error = transport(&server.url, 4)
        .fetch_blob(whole(&Digest::of(b"0123456789")))
        .await
        .unwrap_err();

    assert_eq!(error, TransportError::FrameTooLarge { limit: 4, actual: 10 });
}

#[tokio::test]
async fn test_fetch_caps_a_chunked_blob_without_reading_it_all() {
    // A streaming body carries no Content-Length and never ends. The cap must reject it from the running
    // total the moment it crosses the limit, not by trusting an advertised length.
    let router = serving(|| {
        let stream = futures_util::stream::repeat_with(|| {
            Ok::<_, std::convert::Infallible>(bytes::Bytes::from_static(&[b'x'; 64]))
        });
        Body::from_stream(stream).into_response()
    });
    let server = TestServer::start(router).await;

    let error = transport(&server.url, 16)
        .fetch_blob(whole(&Digest::of(b"unused")))
        .await
        .unwrap_err();

    let TransportError::FrameTooLarge { limit, actual } = error else {
        panic!("expected a frame-too-large rejection, got {error:?}");
    };
    assert_eq!(limit, 16);
    assert!(actual > 16);
}

#[tokio::test]
async fn test_fetch_maps_a_missing_blob_to_not_found() {
    let requested = Digest::of(b"absent");
    let router = serving(|| StatusCode::NOT_FOUND.into_response());
    let server = TestServer::start(router).await;

    let error = transport(&server.url, 1024)
        .fetch_blob(whole(&requested))
        .await
        .unwrap_err();

    assert_eq!(
        error,
        TransportError::BlobNotFound {
            digest: requested.as_str().to_owned(),
        },
    );
}

#[tokio::test]
async fn test_fetch_maps_an_auth_reject_to_unauthenticated() {
    let router = serving(|| StatusCode::UNAUTHORIZED.into_response());
    let server = TestServer::start(router).await;

    let error = transport(&server.url, 1024)
        .fetch_blob(whole(&Digest::of(b"x")))
        .await
        .unwrap_err();

    assert_eq!(error, TransportError::Unauthenticated);
}

#[tokio::test]
async fn test_fetch_maps_a_transient_server_error_to_a_retryable_arm() {
    for status in [
        StatusCode::INTERNAL_SERVER_ERROR,
        StatusCode::BAD_GATEWAY,
        StatusCode::SERVICE_UNAVAILABLE,
    ] {
        let router = serving(move || status.into_response());
        let server = TestServer::start(router).await;

        let error = transport(&server.url, 1024)
            .fetch_blob(whole(&Digest::of(b"x")))
            .await
            .unwrap_err();

        assert_eq!(
            error,
            TransportError::ServerError {
                status: status.as_u16()
            },
            "{status}"
        );
        assert!(error.is_retryable(), "{status}");
    }
}

#[tokio::test]
async fn test_fetch_maps_a_capability_gap_and_client_error_to_terminal_bad_status() {
    for status in [
        StatusCode::NOT_IMPLEMENTED,
        StatusCode::BAD_REQUEST,
        StatusCode::FORBIDDEN,
    ] {
        let router = serving(move || status.into_response());
        let server = TestServer::start(router).await;

        let error = transport(&server.url, 1024)
            .fetch_blob(whole(&Digest::of(b"x")))
            .await
            .unwrap_err();

        assert_eq!(
            error,
            TransportError::BadStatus {
                status: status.as_u16()
            },
            "{status}"
        );
        assert!(!error.is_retryable(), "{status}");
    }
}

#[tokio::test]
async fn test_fetch_maps_a_refused_connection_to_disconnected() {
    // Nothing listens on this port, so the connection is refused before any byte transfers.
    let error = transport("http://127.0.0.1:1/", 1024)
        .fetch_blob(whole(&Digest::of(b"x")))
        .await
        .unwrap_err();

    assert_eq!(error, TransportError::Disconnected);
}

#[tokio::test]
async fn test_fetch_maps_a_deadline_to_timeout() {
    let router = serving(|| {
        let stream = futures_util::stream::pending::<Result<bytes::Bytes, std::convert::Infallible>>();
        Body::from_stream(stream).into_response()
    });
    let server = TestServer::start(router).await;
    let slow = HttpBlobTransport::new(&server.url, TOKEN, limits(1024), Duration::from_millis(150)).unwrap();

    let error = slow.fetch_blob(whole(&Digest::of(b"x"))).await.unwrap_err();

    assert_eq!(error, TransportError::Timeout);
}

#[tokio::test]
async fn test_fetch_maps_a_mid_body_drop_to_disconnected() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    // The response promises a full body, sends part of it, then drops the connection. Reading the body
    // fails partway on an early EOF, which the transport maps to a retryable disconnect rather than
    // mistaking the short read for a complete blob.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).await;
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\npartial-body")
                .await;
        }
    });

    let error = transport(&format!("http://{address}/"), 1024)
        .fetch_blob(whole(&Digest::of(b"x")))
        .await
        .unwrap_err();

    assert_eq!(error, TransportError::Disconnected);
    task.abort();
}
