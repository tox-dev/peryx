//! Turn one ranged fetch result into a verified-length [`BlobPiece`], the adapter between a ranged
//! [`BlobTransport`](crate::blob) fetch and [`reassemble_verified`](crate::blob_reassembly).
//!
//! The fetch driver asks a peer for a byte range and gets back the bytes it served. Before those bytes
//! join the pieces [`reassemble_verified`](crate::blob_reassembly) checks, this pairs them with the
//! range that was asked for and fails closed when they do not match, so a peer that returns the wrong
//! bytes for a range is caught here rather than deep in reassembly.

use bytes::Bytes;

use crate::blob::ByteRange;
use crate::blob_reassembly::BlobPiece;

/// Why a ranged fetch result could not become a [`BlobPiece`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PieceError {
    /// The fetched body is not the length its range declares: a peer served the wrong bytes for the
    /// range, or the range ran past `usize` and saturated to a length no body can match.
    LengthMismatch { expected: usize, actual: usize },
}

/// Pair a requested byte range with the bytes a ranged fetch returned into a [`BlobPiece`].
///
/// `offset` and `length` arrive as wire `u64` and convert to the `usize` a [`ByteRange`] holds through
/// a checked conversion, never an `as` cast: a value past `usize` saturates to `usize::MAX` instead of
/// truncating, so it cannot match any real body and fails the length check rather than smuggling a
/// short range through. The body must be exactly `length` bytes; a shorter or longer one means the peer
/// served the wrong content for the range and fails closed here, before [`reassemble_verified`] runs.
///
/// # Errors
/// [`PieceError::LengthMismatch`] when the body length does not equal the requested range length.
///
/// [`reassemble_verified`]: crate::blob_reassembly::reassemble_verified
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
