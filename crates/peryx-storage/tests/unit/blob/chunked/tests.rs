use std::num::NonZeroU64;

use rstest::rstest;

use super::{CHUNK_BYTES, ChunkedDigest, ChunkedDigestBuilder};
use crate::blob::Digest;

fn nz(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value).unwrap()
}

fn body(len: usize) -> Vec<u8> {
    (0..len).map(|index| index.to_le_bytes()[0]).collect()
}

fn built(bytes: &[u8], chunk: u64, piece: usize) -> ChunkedDigest {
    let mut builder = ChunkedDigestBuilder::new(nz(chunk));
    for slice in bytes.chunks(piece.max(1)) {
        builder.update(slice);
    }
    builder.finish()
}

#[rstest]
// Blobs shorter than, equal to, and longer than the chunk, with a ragged last chunk, fed through the
// streaming builder in pieces that both align to and straddle the chunk boundary.
#[case::empty(0, 3)]
#[case::partial(2, 3)]
#[case::exact(4, 3)]
#[case::one_and_a_bit(5, 3)]
#[case::aligned_pieces(8, 4)]
#[case::straddling_pieces(10, 3)]
#[case::single_byte_pieces(9, 1)]
fn test_streaming_builder_agrees_with_the_whole_computation(#[case] len: usize, #[case] piece: usize) {
    let bytes = body(len);
    assert_eq!(built(&bytes, 4, piece), ChunkedDigest::of(&bytes, nz(4)));
}

#[test]
fn test_each_chunk_digest_is_the_sha256_of_its_span() {
    let bytes = body(10);
    let chunked = ChunkedDigest::of(&bytes, nz(4));

    assert_eq!(chunked.len(), 3);
    assert_eq!(chunked.digests[0], Digest::of(&bytes[0..4]).as_str());
    assert_eq!(chunked.digests[1], Digest::of(&bytes[4..8]).as_str());
    assert_eq!(chunked.digests[2], Digest::of(&bytes[8..10]).as_str());
}

#[test]
fn test_a_chunk_verifies_against_its_own_bytes_and_rejects_others() {
    let bytes = body(10);
    let chunked = ChunkedDigest::of(&bytes, nz(4));

    assert!(chunked.verify_chunk(1, &bytes[4..8]));
    assert!(
        !chunked.verify_chunk(1, &bytes[0..4]),
        "another chunk's bytes must not verify"
    );
    assert!(
        !chunked.verify_chunk(9, &bytes[0..4]),
        "an index past the last chunk never verifies"
    );
}

#[test]
fn test_range_covers_each_chunk_and_stops_past_the_last() {
    let chunked = ChunkedDigest::of(&body(10), nz(4));

    assert_eq!(chunked.range(0, 10), Some(0..4));
    assert_eq!(chunked.range(1, 10), Some(4..8));
    assert_eq!(
        chunked.range(2, 10),
        Some(8..10),
        "the last range is clamped to the total length"
    );
    assert_eq!(chunked.range(3, 10), None);
}

#[test]
fn test_the_empty_blob_has_no_chunks() {
    let chunked = ChunkedDigest::of(&[], nz(4));

    assert!(chunked.is_empty());
    assert_eq!(chunked.len(), 0);
    assert_eq!(chunked.range(0, 0), None);
    assert!(!chunked.verify_chunk(0, &[]));
}

#[test]
fn test_the_default_chunk_size_rides_on_the_computed_digest() {
    let chunked = ChunkedDigest::of(&body(3), CHUNK_BYTES);
    assert_eq!(chunked.chunk_size, CHUNK_BYTES.get());
    assert_eq!(chunked.len(), 1);
}
