use crate::blob::ByteRange;
use crate::blob_pull::{ChunkFailure, ChunkUnavailable, chunk_ranges};
use crate::peer::TransportError;

fn range(offset: usize, length: usize) -> ByteRange {
    ByteRange { offset, length }
}

#[test]
fn test_chunk_ranges_tiles_the_blob_into_consecutive_ranges() {
    assert!(chunk_ranges(0, 4).is_empty());
    assert_eq!(chunk_ranges(3, 4), [range(0, 3)]);
    assert_eq!(chunk_ranges(8, 4), [range(0, 4), range(4, 4)]);
    assert_eq!(chunk_ranges(10, 4), [range(0, 4), range(4, 4), range(8, 2)]);
}

#[test]
fn test_chunk_unavailable_keeps_only_the_recoverable_transport_failures() {
    let unavailable = ChunkUnavailable {
        index: 0,
        range: range(0, 4),
        failures: vec![
            (0, ChunkFailure::Transport(TransportError::Timeout)),
            (1, ChunkFailure::DigestMismatch),
            (2, ChunkFailure::WrongLength { expected: 4, got: 2 }),
        ],
    };

    assert_eq!(unavailable.transport_failures(), vec![(0, TransportError::Timeout)]);
}
