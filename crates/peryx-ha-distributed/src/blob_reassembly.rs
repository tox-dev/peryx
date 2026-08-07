//! Reassembling a blob from ranged pieces and verifying it against its digest.
//!
//! A resumable or multi-source fetch pulls a blob as separate byte ranges, possibly from more than one
//! placement and out of order, and none of a piece's bytes are trusted until the whole blob hashes to
//! its expected digest. This is that check: it stitches the pieces into the blob and verifies the digest
//! before any byte is handed on, so a peer cannot substitute content for a range it served.
//!
//! The pieces must tile `[0, total_length)` exactly. A piece whose bytes do not fill its declared range,
//! a hole, an overlap, or a covered length that misses the total each fail before the digest is
//! computed, so a malformed set never reaches the hash. A [`DigestMismatch`](ReassemblyError::DigestMismatch)
//! reports only the expected and computed digests, never which piece diverged, so a tampering peer
//! cannot narrow down the byte it must corrupt.
//!
//! This is the pure verification core. The transport that fetches each range, the scheduler that drives
//! the fetch, and the retry and circuit-breaking around a failed source are deferred wiring.

use bytes::{Bytes, BytesMut};
use peryx_storage::blob::Digest;

use crate::blob::ByteRange;

/// One fetched range of a blob: the range it claims to cover and the bytes returned for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobPiece {
    pub range: ByteRange,
    pub bytes: Bytes,
}

/// Why a set of pieces could not be reassembled into a verified blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReassemblyError {
    /// A piece's byte count does not match the length its range declares.
    PieceLength {
        offset: usize,
        declared: usize,
        received: usize,
    },
    /// No piece covers the byte at `at`, so the blob has a hole.
    Gap { at: usize },
    /// Two pieces both cover the byte at `at`.
    Overlap { at: usize },
    /// The pieces cover a length other than the expected total.
    UnexpectedTotal { expected: usize, assembled: usize },
    /// The reassembled bytes do not hash to the expected digest. Names only the digests, not the piece.
    DigestMismatch { expected: String, received: String },
}

/// Reassemble `pieces` into the blob of `total_length` bytes and verify it against `expected`.
///
/// The pieces may arrive in any order and from any source. They must tile `[0, total_length)` exactly:
/// each piece's bytes match its declared range length, and the ranges leave no gap and no overlap. A
/// structural fault is reported before the digest is computed. On success the whole blob is returned;
/// otherwise the reassembled bytes hashed to something other than `expected` and the result is
/// [`ReassemblyError::DigestMismatch`].
///
/// # Errors
/// Returns a [`ReassemblyError`] when a piece's length is wrong, the ranges do not tile the blob, or the
/// verified digest does not match.
pub fn reassemble_verified(
    pieces: &[BlobPiece],
    total_length: usize,
    expected: &Digest,
) -> Result<Bytes, ReassemblyError> {
    for piece in pieces {
        if piece.bytes.len() != piece.range.length {
            return Err(ReassemblyError::PieceLength {
                offset: piece.range.offset,
                declared: piece.range.length,
                received: piece.bytes.len(),
            });
        }
    }
    let mut ordered: Vec<&BlobPiece> = pieces.iter().collect();
    ordered.sort_by_key(|piece| piece.range.offset);
    let mut assembled = BytesMut::with_capacity(total_length);
    let mut cursor = 0;
    for piece in ordered {
        if piece.range.offset > cursor {
            return Err(ReassemblyError::Gap { at: cursor });
        }
        if piece.range.offset < cursor {
            return Err(ReassemblyError::Overlap { at: piece.range.offset });
        }
        assembled.extend_from_slice(&piece.bytes);
        cursor += piece.range.length;
    }
    if cursor != total_length {
        return Err(ReassemblyError::UnexpectedTotal {
            expected: total_length,
            assembled: cursor,
        });
    }
    let assembled = assembled.freeze();
    let actual = Digest::of(&assembled);
    if actual != *expected {
        return Err(ReassemblyError::DigestMismatch {
            expected: expected.as_str().to_owned(),
            received: actual.as_str().to_owned(),
        });
    }
    Ok(assembled)
}
