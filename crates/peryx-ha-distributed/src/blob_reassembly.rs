//! Ranged pieces remain untrusted until they tile `[0, total_length)` and the whole blob matches its
//! expected digest. Structural errors take precedence over digest verification.

use bytes::{Bytes, BytesMut};
use peryx_storage::blob::Digest;

use crate::blob::ByteRange;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobPiece {
    pub range: ByteRange,
    pub bytes: Bytes,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReassemblyError {
    PieceLength {
        offset: usize,
        declared: usize,
        received: usize,
    },
    Gap {
        at: usize,
    },
    Overlap {
        at: usize,
    },
    UnexpectedTotal {
        expected: usize,
        assembled: usize,
    },
    /// Reports only the expected and received digests, without attributing a corrupt piece.
    DigestMismatch {
        expected: String,
        received: String,
    },
}

/// Accepts pieces in any order, but requires exact tiling and matching declared lengths before hashing.
///
/// # Errors
/// Returns [`ReassemblyError`] for a length mismatch, gap, overlap, unexpected total, or digest mismatch.
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
