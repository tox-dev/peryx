use std::num::{NonZeroU64, NonZeroUsize};
use std::time::Duration;

use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use peryx_storage::blob::{BlobStorage, Digest};

use crate::blob::{BlobRequest, BlobTransport, ByteRange};
use crate::blob_http::{HttpBlobError, HttpBlobTransport};
use crate::peer::{TransferLimits, TransportError};
use crate::support::http_contract;

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

fn transport(url: &str, cap: u64) -> HttpBlobTransport {
    HttpBlobTransport::new(url, TOKEN, limits(cap), Duration::from_secs(5)).unwrap()
}

fn bytes_response(body: &'static [u8]) -> axum::Router {
    http_contract::fixed_get(BLOB_ROUTE, move || (StatusCode::OK, body).into_response())
}

#[test]
fn test_configuration_contract() {
    http_contract::assert_configuration(
        |base, token| HttpBlobTransport::new(base, token, limits(64), Duration::from_secs(1)).map(|_| ()),
        |error| matches!(error, HttpBlobError::EmptyToken),
        |error| matches!(error, HttpBlobError::InvalidBase(_)),
    );
}

#[test]
fn test_debug_names_the_blob_endpoint_without_the_token() {
    http_contract::assert_redacted(
        &transport("http://peer.example/root", 64),
        TOKEN,
        &["HttpBlobTransport", "blobs_url"],
    );
}

#[tokio::test]
async fn test_fetch_verifies_and_returns_a_whole_blob() {
    let content = b"the real blob bytes";
    let digest = Digest::of(content);
    let fetched = http_contract::run_nested(bytes_response(content), |base| async move {
        transport(&base, 1024).fetch_blob(whole(&digest)).await.unwrap()
    })
    .await;

    assert_eq!(fetched, content);
}

#[tokio::test]
async fn test_blob_size_uses_a_bodyless_availability_request() {
    let content = b"the real blob bytes";
    let dir = tempfile::tempdir().unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let digest = blobs.put_bytes(content).await.unwrap();
    let router = crate::primary_router(
        "primary",
        TOKEN,
        crate::support::distributed_meta(dir.path().join("peryx.redb")),
        blobs,
    )
    .unwrap();
    let size = http_contract::run(router, |base| async move {
        transport(&base, 1024).blob_size(&digest).await.unwrap()
    })
    .await;

    assert_eq!(size, Some(content.len() as u64));
}

#[tokio::test]
async fn test_fetch_rejects_a_whole_blob_that_does_not_match_its_digest() {
    let requested = Digest::of(b"the real blob");
    let substituted = b"substituted bytes";
    let expected = requested.as_str().to_owned();
    let error = http_contract::run(bytes_response(substituted), |base| async move {
        transport(&base, 1024).fetch_blob(whole(&requested)).await.unwrap_err()
    })
    .await;

    assert_eq!(
        error,
        TransportError::DigestMismatch {
            expected,
            actual: Digest::of(substituted).as_str().to_owned(),
        },
    );
}

const SLICE: &[u8] = b"a slice of it";

fn slice_range() -> ByteRange {
    ByteRange { offset: 4, length: 13 }
}

fn ranged(digest: Digest) -> BlobRequest {
    BlobRequest {
        digest,
        range: Some(slice_range()),
    }
}

fn range_response(status: StatusCode, content_range: Option<&'static str>) -> axum::Router {
    http_contract::fixed_get(BLOB_ROUTE, move || {
        let mut response = (status, SLICE).into_response();
        if let Some(value) = content_range {
            response
                .headers_mut()
                .insert(header::CONTENT_RANGE, value.parse().unwrap());
        }
        response
    })
}

#[tokio::test]
async fn test_fetch_returns_a_range_unverified() {
    // Partial ranges cannot verify the whole digest; commit verifies the reassembled blob.
    let requested = Digest::of(b"the whole blob is longer than this");
    let fetched = http_contract::run(
        range_response(StatusCode::PARTIAL_CONTENT, Some("bytes 4-16/34")),
        |base| async move { transport(&base, 1024).fetch_blob(ranged(requested)).await.unwrap() },
    )
    .await;

    assert_eq!(fetched, SLICE);
}

#[rstest::rstest]
#[case::whole_response(StatusCode::OK, Some("bytes 4-16/34"), "")]
#[case::other_window(StatusCode::PARTIAL_CONTENT, Some("bytes 0-12/34"), "bytes 0-12/34")]
#[case::absent_header(StatusCode::PARTIAL_CONTENT, None, "")]
#[case::no_complete_length(StatusCode::PARTIAL_CONTENT, Some("bytes 4-16"), "bytes 4-16")]
#[tokio::test]
async fn test_fetch_rejects_a_range_the_peer_did_not_serve(
    #[case] status: StatusCode,
    #[case] content_range: Option<&'static str>,
    #[case] actual: &str,
) {
    let requested = Digest::of(b"the whole blob is longer than this");
    let error = http_contract::run(range_response(status, content_range), |base| async move {
        transport(&base, 1024).fetch_blob(ranged(requested)).await.unwrap_err()
    })
    .await;

    assert_eq!(
        error,
        TransportError::RangeMismatch {
            expected: "bytes 4-16".to_owned(),
            actual: actual.to_owned(),
        }
    );
}

#[tokio::test]
async fn test_fetch_maps_a_missing_blob_to_not_found() {
    let requested = Digest::of(b"absent");
    let digest = requested.as_str().to_owned();
    let error = http_contract::run(
        http_contract::fixed_get(BLOB_ROUTE, || {
            (StatusCode::NOT_FOUND, [("x-peryx-blob-result", "not-found")]).into_response()
        }),
        |base| async move { transport(&base, 1024).fetch_blob(whole(&requested)).await.unwrap_err() },
    )
    .await;

    assert_eq!(error, TransportError::BlobNotFound { digest },);
}

#[tokio::test]
async fn test_blob_size_reports_a_missing_digest_without_route_ambiguity() {
    let requested = Digest::of(b"absent");
    let size = http_contract::run(
        http_contract::fixed_get(BLOB_ROUTE, || {
            (StatusCode::NOT_FOUND, [("x-peryx-blob-result", "not-found")]).into_response()
        }),
        |base| async move { transport(&base, 1024).blob_size(&requested).await.unwrap() },
    )
    .await;

    assert_eq!(size, None);
}

#[tokio::test]
async fn test_fetch_distinguishes_a_peer_without_the_blob_route() {
    let requested = Digest::of(b"absent route");
    let error = http_contract::run(axum::Router::new(), |base| async move {
        transport(&base, 1024).fetch_blob(whole(&requested)).await.unwrap_err()
    })
    .await;

    assert_eq!(error, TransportError::BadStatus { status: 404 });
}

#[tokio::test]
async fn test_blob_size_distinguishes_a_peer_without_the_blob_route() {
    let requested = Digest::of(b"absent route");
    let error = http_contract::run(axum::Router::new(), |base| async move {
        transport(&base, 1024).blob_size(&requested).await.unwrap_err()
    })
    .await;

    assert_eq!(error, TransportError::BadStatus { status: 404 });
}

#[rstest::rstest]
#[case::unauthenticated(StatusCode::UNAUTHORIZED, TransportError::Unauthenticated)]
#[case::server_error(StatusCode::SERVICE_UNAVAILABLE, TransportError::ServerError {
            status: 503,
            retry_after: None,
        })]
#[case::bad_status(StatusCode::BAD_REQUEST, TransportError::BadStatus { status: 400 })]
#[tokio::test]
async fn test_fetch_maps_http_errors(#[case] status: StatusCode, #[case] expected: TransportError) {
    let digest = Digest::of(b"artifact");
    http_contract::assert_mapping(
        http_contract::fixed_get(BLOB_ROUTE, move || status.into_response()),
        |base| async move { transport(&base, 1024).fetch_blob(whole(&digest)).await },
        Err(expected),
    )
    .await;
}
