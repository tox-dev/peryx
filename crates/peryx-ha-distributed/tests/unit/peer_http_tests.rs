use std::num::{NonZeroU64, NonZeroUsize};
use std::time::Duration;

use axum::Json;
use axum::body::Body;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;

use crate::peer::{BatchRequest, PeerTransport, TransferLimits, TransportError};
use crate::peer_http::{HttpPeerError, HttpPeerTransport};
use crate::protocol::{Change, ChangePage, PROTOCOL_VERSION};
use crate::support::http_contract;

const CHANGES_ROUTE: &str = "/+replication/v1/changes";
const TOKEN: &str = "secret";

fn change(serial: u64) -> Change {
    Change {
        serial,
        event: serial.to_le_bytes().to_vec(),
        metadata: Vec::new(),
        blobs: Vec::new(),
    }
}

fn sample_page() -> ChangePage {
    ChangePage {
        version: PROTOCOL_VERSION,
        source: "primary-a".to_owned(),
        after: 0,
        current_serial: 2,
        changes: vec![change(1), change(2)],
    }
}

fn limits(max_operations: usize, max_encoded_bytes: u64) -> TransferLimits {
    TransferLimits {
        max_operations: NonZeroUsize::new(max_operations).unwrap(),
        max_encoded_bytes: NonZeroU64::new(max_encoded_bytes).unwrap(),
    }
}

fn request(after: u64, max_operations: usize) -> BatchRequest {
    BatchRequest {
        after,
        max_operations: NonZeroUsize::new(max_operations).unwrap(),
    }
}

fn transport(base: &str, transfer: TransferLimits) -> HttpPeerTransport {
    HttpPeerTransport::new(base, TOKEN, transfer, Duration::from_secs(5)).unwrap()
}

#[test]
fn test_configuration_contract() {
    http_contract::assert_configuration(
        |base, token| HttpPeerTransport::new(base, token, limits(8, 4096), Duration::from_secs(5)).map(|_| ()),
        |error| matches!(error, HttpPeerError::EmptyToken),
        |error| matches!(error, HttpPeerError::InvalidBase(_)),
    );
}

#[test]
fn test_debug_names_the_change_endpoint_without_the_token() {
    http_contract::assert_redacted(
        &transport("http://peer.example/root", limits(1, 64)),
        TOKEN,
        &["HttpPeerTransport", "changes_url"],
    );
}

#[tokio::test]
async fn test_fetch_rejects_a_request_over_the_operation_bound() {
    let transport = transport("http://127.0.0.1:1/", limits(2, 4096));

    let error = transport.fetch_batch(request(0, 5)).await.unwrap_err();

    assert_eq!(error, TransportError::TooManyOperations { limit: 2, actual: 5 });
}

#[tokio::test]
async fn test_fetch_parses_a_valid_batch_from_a_nested_base() {
    let frame = http_contract::run_nested(
        http_contract::fixed_get(CHANGES_ROUTE, || Json(sample_page()).into_response()),
        |base| async move {
            transport(&base, limits(256, 4 << 20))
                .fetch_batch(request(0, 10))
                .await
                .unwrap()
        },
    )
    .await;

    assert_eq!(frame.frontier(), ("primary-a", 2));
    assert_eq!(frame.page().changes.len(), 2);
    assert_eq!(frame.page().changes[0].serial, 1);
}

#[tokio::test]
async fn test_fetch_preserves_wire_length_at_count_and_byte_limits() {
    let body = format!(" \n{}\t ", serde_json::to_string(&sample_page()).unwrap());
    let byte_limit = body.len() as u64;
    let response_body = body.clone();
    let frame = http_contract::run(
        http_contract::fixed_get(CHANGES_ROUTE, move || Response::new(Body::from(response_body.clone()))),
        |base| async move {
            transport(&base, limits(2, byte_limit))
                .fetch_batch(request(0, 2))
                .await
                .unwrap()
        },
    )
    .await;

    assert_eq!(frame.page(), &sample_page());
    assert_eq!(frame.encoded_len(), byte_limit);
}

#[tokio::test]
async fn test_fetch_rejects_a_page_over_the_requested_count() {
    http_contract::assert_mapping(
        http_contract::fixed_get(CHANGES_ROUTE, || Json(sample_page()).into_response()),
        |base| async move { transport(&base, limits(2, 4096)).fetch_batch(request(0, 1)).await },
        Err(TransportError::TooManyOperations { limit: 1, actual: 2 }),
    )
    .await;
}

#[tokio::test]
async fn test_fetch_applies_the_streaming_byte_cap_before_decode() {
    let encoded = serde_json::to_vec(&sample_page()).unwrap();
    let byte_limit = encoded.len() as u64;
    let response_body = encoded.clone();

    let error = http_contract::run(
        http_contract::fixed_get(CHANGES_ROUTE, move || {
            Response::new(Body::from_stream(futures_util::stream::iter([
                Ok::<_, std::io::Error>(Bytes::from(response_body.clone())),
                Ok(Bytes::from_static(b" ")),
            ])))
        }),
        |base| async move {
            transport(&base, limits(2, byte_limit))
                .fetch_batch(request(0, 2))
                .await
                .unwrap_err()
        },
    )
    .await;

    assert_eq!(
        error,
        TransportError::FrameTooLarge {
            limit: byte_limit,
            actual: byte_limit + 1,
        }
    );
}

#[tokio::test]
async fn test_fetch_names_the_record_the_writer_could_not_page() {
    http_contract::assert_mapping(
        http_contract::fixed_get(CHANGES_ROUTE, || {
            (StatusCode::PAYLOAD_TOO_LARGE, "journal record 4 fills a page alone").into_response()
        }),
        |base| async move { transport(&base, limits(256, 4096)).fetch_batch(request(3, 10)).await },
        Err(TransportError::RecordTooLarge { serial: 4, limit: 4096 }),
    )
    .await;
}

#[tokio::test]
async fn test_fetch_maps_a_non_page_body_to_malformed() {
    http_contract::assert_mapping(
        http_contract::fixed_get(CHANGES_ROUTE, || (StatusCode::OK, "not a change page").into_response()),
        |base| async move { transport(&base, limits(256, 4 << 20)).fetch_batch(request(0, 10)).await },
        Err(TransportError::Malformed),
    )
    .await;
}

#[rstest::rstest]
#[case::unauthenticated(StatusCode::UNAUTHORIZED, TransportError::Unauthenticated)]
#[case::server_error(StatusCode::SERVICE_UNAVAILABLE, TransportError::ServerError { status: 503 })]
#[case::bad_status(StatusCode::BAD_REQUEST, TransportError::BadStatus { status: 400 })]
#[tokio::test]
async fn test_fetch_maps_http_errors(#[case] status: StatusCode, #[case] expected: TransportError) {
    http_contract::assert_mapping(
        http_contract::fixed_get(CHANGES_ROUTE, move || status.into_response()),
        |base| async move { transport(&base, limits(256, 4 << 20)).fetch_batch(request(0, 10)).await },
        Err(expected),
    )
    .await;
}
