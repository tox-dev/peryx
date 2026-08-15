//! Rejects ranged response bodies whose lengths do not match the requested ranges before reassembly.

use bytes::Bytes;

use crate::blob::ByteRange;
use crate::blob_reassembly::BlobPiece;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PieceError {
    /// The body length differs from the request, or a wire range exceeded `usize`.
    LengthMismatch { expected: usize, actual: usize },
}

/// Wire values beyond `usize::MAX` saturate instead of truncating, forcing the length check to fail.
///
/// # Errors
/// [`PieceError::LengthMismatch`] when the body length does not equal the requested range length.
pub fn blob_piece(offset: u64, length: u64, bytes: Vec<u8>) -> Result<BlobPiece, PieceError> {
    let offset = usize::try_from(offset).unwrap_or(usize::MAX);
    let length = usize::try_from(length).unwrap_or(usize::MAX);
    if bytes.len() != length {
        return Err(PieceError::LengthMismatch {
            expected: length,
            actual: bytes.len(),
        });
    }
    Ok(BlobPiece {
        range: ByteRange { offset, length },
        bytes: Bytes::from(bytes),
    })
}
