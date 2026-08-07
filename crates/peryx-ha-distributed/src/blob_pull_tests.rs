use std::num::NonZeroU64;

use async_trait::async_trait;
use bytes::Bytes;
use peryx_storage::blob::{ChunkedDigest, Digest};

use crate::blob::{BlobRequest, BlobTransport, ByteRange};
use crate::blob_piece::PieceError;
use crate::blob_pull::{
    ChunkFailure, ChunkUnavailable, PullError, chunk_ranges, pull_chunk_verified, pull_ranged, pull_ranged_blob,
};
use crate::blob_reassembly::ReassemblyError;
use crate::peer::TransportError;

enum Behavior {
    Serve(Bytes),
    Fail(TransportError),
    Garbage,
}

struct Stub(Behavior);

#[async_trait]
impl BlobTransport for Stub {
    async fn fetch_blob(&self, request: BlobRequest) -> Result<Vec<u8>, TransportError> {
        match &self.0 {
            Behavior::Serve(content) => {
                let range = request.range.expect("pull_ranged always requests a range");
                let start = range.offset.min(content.len());
                let end = start.saturating_add(range.length).min(content.len());
                Ok(content.slice(start..end).to_vec())
            }
            Behavior::Fail(error) => Err(error.clone()),
            // Right length, wrong content: a `Verified` peer whose range bytes pass the length check but
            // poison the reassembled digest.
            Behavior::Garbage => {
                let range = request.range.expect("pull_ranged always requests a range");
                Ok(vec![0xFF; range.length])
            }
        }
    }
}

fn serve(content: &'static [u8]) -> Stub {
    Stub(Behavior::Serve(Bytes::from_static(content)))
}

fn fail(error: TransportError) -> Stub {
    Stub(Behavior::Fail(error))
}

fn garbage() -> Stub {
    Stub(Behavior::Garbage)
}

fn range(offset: usize, length: usize) -> ByteRange {
    ByteRange { offset, length }
}

#[tokio::test]
async fn test_pull_ranged_assembles_from_the_first_source() {
    let content = b"0123456789";
    let expected = Digest::of(content);
    let source = serve(content);

    let bytes = pull_ranged(&[&source], &expected, &[range(0, 4), range(4, 6)], 10, &expected).await;

    assert_eq!(bytes, Ok(Bytes::from_static(content)));
}

#[tokio::test]
async fn test_pull_ranged_falls_to_the_next_source_on_a_retryable_loss() {
    let content = b"0123456789";
    let expected = Digest::of(content);
    let down = fail(TransportError::Timeout);
    let up = serve(content);

    let bytes = pull_ranged(&[&down, &up], &expected, &[range(0, 10)], 10, &expected).await;

    assert_eq!(bytes, Ok(Bytes::from_static(content)));
}

#[tokio::test]
async fn test_pull_ranged_exhausts_every_source_and_collects_their_errors() {
    let expected = Digest::of(b"0123456789");
    let first = fail(TransportError::Timeout);
    let second = fail(TransportError::BlobNotFound {
        digest: expected.as_str().to_owned(),
    });

    let result = pull_ranged(&[&first, &second], &expected, &[range(0, 10)], 10, &expected).await;

    assert_eq!(
        result,
        Err(PullError::Exhausted {
            range: range(0, 10),
            failures: vec![
                (0, TransportError::Timeout),
                (
                    1,
                    TransportError::BlobNotFound {
                        digest: expected.as_str().to_owned(),
                    },
                ),
            ],
        })
    );
}

#[tokio::test]
async fn test_pull_ranged_falls_through_a_non_retryable_source_error() {
    let content = b"0123456789";
    let expected = Digest::of(content);
    let gone = fail(TransportError::BlobNotFound {
        digest: expected.as_str().to_owned(),
    });
    let backup = serve(content);

    let bytes = pull_ranged(&[&gone, &backup], &expected, &[range(0, 10)], 10, &expected).await;

    assert_eq!(bytes, Ok(Bytes::from_static(content)));
}

#[tokio::test]
async fn test_pull_ranged_rejects_a_blob_that_fails_verification() {
    let source = serve(b"0123456789");
    let wrong = Digest::of(b"a different blob");

    let result = pull_ranged(&[&source], &wrong, &[range(0, 10)], 10, &wrong).await;

    let Err(PullError::Reassembly(ReassemblyError::DigestMismatch { .. })) = result else {
        panic!("expected a digest-mismatch reassembly error, got {result:?}");
    };
}

#[tokio::test]
async fn test_pull_ranged_falls_back_from_a_wrong_content_source() {
    let content = b"0123456789";
    let expected = Digest::of(content);
    let poisoned = garbage();
    let healthy = serve(content);

    let bytes = pull_ranged(&[&poisoned, &healthy], &expected, &[range(0, 10)], 10, &expected).await;

    assert_eq!(bytes, Ok(Bytes::from_static(content)));
}

#[tokio::test]
async fn test_pull_ranged_drops_a_wrong_content_source_across_several_ranges() {
    let content = b"0123456789abcdef";
    let expected = Digest::of(content);
    let poisoned = garbage();
    let healthy = serve(content);

    let bytes = pull_ranged(
        &[&poisoned, &healthy],
        &expected,
        &[range(0, 8), range(8, 8)],
        16,
        &expected,
    )
    .await;

    assert_eq!(bytes, Ok(Bytes::from_static(content)));
}

#[tokio::test]
async fn test_pull_ranged_skips_a_down_source_then_a_wrong_content_source() {
    let content = b"0123456789";
    let expected = Digest::of(content);
    let down = fail(TransportError::Timeout);
    let poisoned = garbage();
    let healthy = serve(content);

    let bytes = pull_ranged(&[&down, &poisoned, &healthy], &expected, &[range(0, 10)], 10, &expected).await;

    assert_eq!(bytes, Ok(Bytes::from_static(content)));
}

#[tokio::test]
async fn test_pull_ranged_rejects_when_every_source_serves_wrong_content() {
    let expected = Digest::of(b"0123456789");

    let result = pull_ranged(&[&garbage(), &garbage()], &expected, &[range(0, 10)], 10, &expected).await;

    let Err(PullError::Reassembly(ReassemblyError::DigestMismatch { .. })) = result else {
        panic!("expected a digest-mismatch reassembly error, got {result:?}");
    };
}

#[tokio::test]
async fn test_pull_ranged_rejects_a_wrong_sized_range_body() {
    let content = b"0123";
    let expected = Digest::of(content);
    let source = serve(content);

    let result = pull_ranged(&[&source], &expected, &[range(0, 6)], 6, &expected).await;

    assert_eq!(
        result,
        Err(PullError::Piece(PieceError::LengthMismatch { expected: 6, actual: 4 }))
    );
}

#[tokio::test]
async fn test_pull_ranged_verifies_the_empty_blob_from_zero_ranges() {
    let expected = Digest::of(b"");
    let source = serve(b"");

    let bytes = pull_ranged(&[&source], &expected, &[], 0, &expected).await;

    assert_eq!(bytes, Ok(Bytes::new()));
}

#[tokio::test]
async fn test_pull_ranged_blob_chunks_and_verifies_the_whole() {
    let content = b"0123456789";
    let expected = Digest::of(content);
    let source = serve(content);

    let bytes = pull_ranged_blob(&[&source], &expected, content.len()).await;

    assert_eq!(bytes, Ok(Bytes::from_static(content)));
}

#[tokio::test]
async fn test_pull_ranged_blob_falls_through_to_the_next_source() {
    let content = b"0123456789";
    let expected = Digest::of(content);
    let down = fail(TransportError::Timeout);
    let up = serve(content);

    let bytes = pull_ranged_blob(&[&down, &up], &expected, content.len()).await;

    assert_eq!(bytes, Ok(Bytes::from_static(content)));
}

#[tokio::test]
async fn test_pull_ranged_blob_verifies_the_empty_blob() {
    let expected = Digest::of(b"");
    let source = serve(b"");

    let bytes = pull_ranged_blob(&[&source], &expected, 0).await;

    assert_eq!(bytes, Ok(Bytes::new()));
}

fn chunked(content: &[u8], size: u64) -> ChunkedDigest {
    ChunkedDigest::of(content, NonZeroU64::new(size).unwrap())
}

#[tokio::test]
async fn test_pull_chunk_verified_returns_a_verified_chunk() {
    let content = b"0123456789";
    let digest = Digest::of(content);
    let chunks = chunked(content, 4);
    let source = serve(content);

    let bytes = pull_chunk_verified(&[&source], &digest, &chunks, 1, 10).await;

    assert_eq!(bytes, Ok(Bytes::from_static(b"4567")));
}

#[tokio::test]
async fn test_pull_chunk_verified_covers_the_ragged_last_chunk() {
    let content = b"0123456789";
    let digest = Digest::of(content);
    let chunks = chunked(content, 4);
    let source = serve(content);

    let bytes = pull_chunk_verified(&[&source], &digest, &chunks, 2, 10).await;

    assert_eq!(bytes, Ok(Bytes::from_static(b"89")));
}

#[tokio::test]
async fn test_pull_chunk_verified_falls_through_a_transport_loss() {
    let content = b"0123456789";
    let digest = Digest::of(content);
    let chunks = chunked(content, 4);
    let down = fail(TransportError::Timeout);
    let up = serve(content);

    let bytes = pull_chunk_verified(&[&down, &up], &digest, &chunks, 0, 10).await;

    assert_eq!(bytes, Ok(Bytes::from_static(b"0123")));
}

#[tokio::test]
async fn test_pull_chunk_verified_falls_through_a_wrong_content_source() {
    let content = b"0123456789";
    let digest = Digest::of(content);
    let chunks = chunked(content, 4);
    let poisoned = garbage();
    let healthy = serve(content);

    let bytes = pull_chunk_verified(&[&poisoned, &healthy], &digest, &chunks, 0, 10).await;

    assert_eq!(bytes, Ok(Bytes::from_static(b"0123")));
}

#[tokio::test]
async fn test_pull_chunk_verified_falls_through_a_wrong_length_source() {
    let content = b"0123456789";
    let digest = Digest::of(content);
    let chunks = chunked(content, 4);
    // A source holding only three bytes under-delivers the four-byte range, so its body is the wrong length.
    let short = serve(b"012");
    let healthy = serve(content);

    let bytes = pull_chunk_verified(&[&short, &healthy], &digest, &chunks, 0, 10).await;

    assert_eq!(bytes, Ok(Bytes::from_static(b"0123")));
}

#[tokio::test]
async fn test_pull_chunk_verified_exhausts_and_classifies_each_failure() {
    let content = b"0123456789";
    let digest = Digest::of(content);
    let chunks = chunked(content, 4);
    let down = fail(TransportError::Timeout);
    let poisoned = garbage();
    let short = serve(b"01");

    let result = pull_chunk_verified(&[&down, &poisoned, &short], &digest, &chunks, 0, 10).await;

    assert_eq!(
        result,
        Err(ChunkUnavailable {
            index: 0,
            range: range(0, 4),
            failures: vec![
                (0, ChunkFailure::Transport(TransportError::Timeout)),
                (1, ChunkFailure::DigestMismatch),
                (2, ChunkFailure::WrongLength { expected: 4, got: 2 }),
            ],
        })
    );
}

#[tokio::test]
async fn test_chunk_unavailable_keeps_only_the_recoverable_transport_failures() {
    let content = b"0123456789";
    let digest = Digest::of(content);
    let chunks = chunked(content, 4);
    let down = fail(TransportError::Timeout);
    let poisoned = garbage();
    let short = serve(b"01");

    let ChunkUnavailable { failures, .. } = pull_chunk_verified(&[&down, &poisoned, &short], &digest, &chunks, 0, 10)
        .await
        .unwrap_err();

    let unavailable = ChunkUnavailable {
        index: 0,
        range: range(0, 4),
        failures,
    };
    assert_eq!(unavailable.transport_failures(), vec![(0, TransportError::Timeout)]);
}

#[test]
fn test_chunk_ranges_tiles_the_blob_into_consecutive_ranges() {
    assert!(chunk_ranges(0, 4).is_empty());
    assert_eq!(chunk_ranges(3, 4), [range(0, 3)]);
    assert_eq!(chunk_ranges(8, 4), [range(0, 4), range(4, 4)]);
    // The last range is short when the length is not a chunk multiple.
    assert_eq!(chunk_ranges(10, 4), [range(0, 4), range(4, 4), range(8, 2)]);
}
