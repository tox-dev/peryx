use bytes::Bytes;

use crate::blob::ByteRange;
use crate::blob_piece::{PieceError, blob_piece};
use crate::blob_reassembly::BlobPiece;

#[test]
fn test_blob_piece_pairs_a_range_with_its_bytes() {
    assert_eq!(
        blob_piece(10, 4, vec![1, 2, 3, 4]),
        Ok(BlobPiece {
            range: ByteRange { offset: 10, length: 4 },
            bytes: Bytes::from(vec![1, 2, 3, 4]),
        })
    );
}

#[test]
fn test_blob_piece_accepts_a_zero_length_range() {
    assert_eq!(
        blob_piece(5, 0, Vec::new()),
        Ok(BlobPiece {
            range: ByteRange { offset: 5, length: 0 },
            bytes: Bytes::new(),
        })
    );
}

#[test]
fn test_blob_piece_rejects_a_short_body() {
    assert_eq!(
        blob_piece(0, 4, vec![1, 2, 3]),
        Err(PieceError::LengthMismatch { expected: 4, actual: 3 })
    );
}

#[test]
fn test_blob_piece_rejects_a_long_body() {
    assert_eq!(
        blob_piece(0, 2, vec![1, 2, 3]),
        Err(PieceError::LengthMismatch { expected: 2, actual: 3 })
    );
}

#[cfg(target_pointer_width = "32")]
#[test]
fn test_blob_piece_rejects_a_range_past_usize() {
    let past_usize = u64::from(u32::MAX) + 4;

    assert_eq!(
        blob_piece(0, past_usize, vec![0, 0, 0]),
        Err(PieceError::LengthMismatch {
            expected: usize::MAX,
            actual: 3,
        })
    );
}
