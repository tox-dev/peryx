use axum::http::HeaderValue;

use super::{ChunkRange, chunk_range};

fn headers(value: HeaderValue) -> axum::http::HeaderMap {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(axum::http::header::CONTENT_RANGE, value);
    headers
}

fn range(spec: &'static str) -> ChunkRange {
    chunk_range(&headers(HeaderValue::from_static(spec)))
}

#[test]
fn test_chunk_range_reads_both_bounds_with_or_without_the_bytes_prefix() {
    assert_eq!(range("5-9"), ChunkRange::Bytes { start: 5, len: 5 });
    assert_eq!(range("bytes 5-9"), ChunkRange::Bytes { start: 5, len: 5 });
}

#[test]
fn test_chunk_range_reads_a_single_byte_range() {
    assert_eq!(range("0-0"), ChunkRange::Bytes { start: 0, len: 1 });
}

#[test]
fn test_chunk_range_rejects_bytes_that_are_not_text() {
    // A `Content-Range` whose bytes are not text at all: the client made a claim nothing can read.
    let opaque = HeaderValue::from_bytes(&[0xff, 0xfe]).expect("bytes are a valid header value");
    assert_eq!(chunk_range(&headers(opaque)), ChunkRange::Malformed);
}

#[test]
fn test_chunk_range_rejects_a_header_without_a_dash() {
    assert_eq!(range("nowhere"), ChunkRange::Malformed);
}

#[test]
fn test_chunk_range_rejects_a_nonnumeric_bound() {
    assert_eq!(range("0-x"), ChunkRange::Malformed);
    assert_eq!(range("x-9"), ChunkRange::Malformed);
}

#[test]
fn test_chunk_range_rejects_an_end_before_its_start() {
    assert_eq!(range("9-5"), ChunkRange::Malformed);
}

#[test]
fn test_chunk_range_rejects_a_span_that_overflows() {
    // `end - start + 1` overflows `u64`: the width cannot be represented, so it is no valid length.
    assert_eq!(range("0-18446744073709551615"), ChunkRange::Malformed);
}

#[test]
fn test_chunk_range_is_absent_without_the_header() {
    assert_eq!(chunk_range(&axum::http::HeaderMap::new()), ChunkRange::Absent);
}

#[test]
fn test_absent_range_admits_any_body_at_any_offset() {
    assert!(ChunkRange::Absent.admits(7, Some(3)));
    assert!(ChunkRange::Absent.admits(0, None));
}

#[test]
fn test_malformed_range_is_never_admitted() {
    assert!(!ChunkRange::Malformed.admits(0, Some(5)));
}

#[test]
fn test_range_admits_a_contiguous_chunk_whose_length_matches() {
    assert!(ChunkRange::Bytes { start: 5, len: 5 }.admits(5, Some(5)));
}

#[test]
fn test_range_refuses_a_chunk_that_does_not_continue_the_session() {
    assert!(!ChunkRange::Bytes { start: 5, len: 5 }.admits(0, Some(5)));
}

#[test]
fn test_range_refuses_a_length_the_body_does_not_carry() {
    // The `Content-Range: 0-999` one-byte `PATCH`: the declared width dwarfs the body it ships.
    assert!(!ChunkRange::Bytes { start: 0, len: 1000 }.admits(0, Some(1)));
    // An empty body carries no bytes for the inclusive range it claims.
    assert!(!ChunkRange::Bytes { start: 0, len: 5 }.admits(0, Some(0)));
    // A streamed body of unknown length cannot be checked against the claim.
    assert!(!ChunkRange::Bytes { start: 0, len: 5 }.admits(0, None));
}
