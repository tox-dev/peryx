use bytes::Bytes;
use peryx_storage::blob::Digest;

use crate::blob::ByteRange;
use crate::blob_reassembly::{BlobPiece, ReassemblyError, reassemble_verified};

const BLOB: &[u8] = b"hello world!";

fn piece(offset: usize, bytes: &[u8]) -> BlobPiece {
    BlobPiece {
        range: ByteRange {
            offset,
            length: bytes.len(),
        },
        bytes: Bytes::copy_from_slice(bytes),
    }
}

#[test]
fn test_reassembles_pieces_into_the_verified_blob() {
    let pieces = [piece(0, b"hello "), piece(6, b"world!")];

    let blob = reassemble_verified(&pieces, BLOB.len(), &Digest::of(BLOB)).unwrap();

    assert_eq!(&blob[..], BLOB);
}

#[test]
fn test_reassembles_pieces_delivered_out_of_order() {
    let pieces = [piece(6, b"world!"), piece(0, b"hello ")];

    let blob = reassemble_verified(&pieces, BLOB.len(), &Digest::of(BLOB)).unwrap();

    assert_eq!(
        &blob[..],
        BLOB,
        "pieces are stitched in offset order, not arrival order"
    );
}

#[test]
fn test_a_tampered_piece_fails_verification() {
    let pieces = [piece(0, b"hello "), piece(6, b"WORLD!")];

    let error = reassemble_verified(&pieces, BLOB.len(), &Digest::of(BLOB)).unwrap_err();

    assert_eq!(
        error,
        ReassemblyError::DigestMismatch {
            expected: Digest::of(BLOB).as_str().to_owned(),
            received: Digest::of(b"hello WORLD!").as_str().to_owned(),
        },
        "same-length tampered content must be caught by the digest, not passed through",
    );
}

#[test]
fn test_a_hole_between_pieces_is_a_gap() {
    let pieces = [piece(0, b"hell"), piece(8, b"rld!")];

    let error = reassemble_verified(&pieces, BLOB.len(), &Digest::of(BLOB)).unwrap_err();

    assert_eq!(error, ReassemblyError::Gap { at: 4 });
}

#[test]
fn test_overlapping_pieces_are_rejected() {
    let pieces = [piece(0, b"hello "), piece(4, b"middle")];

    let error = reassemble_verified(&pieces, BLOB.len(), &Digest::of(BLOB)).unwrap_err();

    assert_eq!(error, ReassemblyError::Overlap { at: 4 });
}

#[test]
fn test_coverage_short_of_the_total_is_unexpected_total() {
    let pieces = [piece(0, b"hello "), piece(6, b"world!")];

    let error = reassemble_verified(&pieces, 16, &Digest::of(BLOB)).unwrap_err();

    assert_eq!(
        error,
        ReassemblyError::UnexpectedTotal {
            expected: 16,
            assembled: 12,
        }
    );
}

#[test]
fn test_coverage_past_the_total_is_unexpected_total() {
    let pieces = [piece(0, BLOB)];

    let error = reassemble_verified(&pieces, 8, &Digest::of(BLOB)).unwrap_err();

    assert_eq!(
        error,
        ReassemblyError::UnexpectedTotal {
            expected: 8,
            assembled: 12,
        }
    );
}

#[test]
fn test_a_piece_shorter_than_its_range_is_a_piece_length_error() {
    let pieces = [BlobPiece {
        range: ByteRange { offset: 0, length: 6 },
        bytes: Bytes::from_static(b"hi"),
    }];

    let error = reassemble_verified(&pieces, 6, &Digest::of(b"hi")).unwrap_err();

    assert_eq!(
        error,
        ReassemblyError::PieceLength {
            offset: 0,
            declared: 6,
            received: 2,
        }
    );
}
