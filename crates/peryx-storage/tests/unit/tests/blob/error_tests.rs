use std::error::Error as _;

use crate::blob::{BlobError, BlobOperation, BlobScanError, Digest};

#[test]
fn test_blob_operations_use_backend_verbs() {
    for (operation, expected) in [
        (BlobOperation::Health, "health"),
        (BlobOperation::Open, "open"),
        (BlobOperation::Head, "head"),
        (BlobOperation::Write, "write"),
        (BlobOperation::Commit, "commit"),
        (BlobOperation::Delete, "delete"),
        (BlobOperation::Verify, "verify"),
        (BlobOperation::List, "list"),
        (BlobOperation::Materialize, "materialize"),
    ] {
        assert_eq!(operation.to_string(), expected);
    }
}

#[test]
fn test_scan_store_error_reports_source() {
    let digest = Digest::of(b"missing");
    let err: BlobScanError<std::io::Error> = BlobError::not_found(&digest).into();
    assert_eq!(err.to_string(), format!("blob {} not found", digest.as_str()));
    assert!(err.source().is_some());
}

#[test]
fn test_non_mismatch_error_returns_no_mismatch() {
    assert_eq!(BlobError::io(std::io::Error::other("disk")).mismatch(), None);
}

#[test]
fn test_non_range_error_returns_no_range_values() {
    assert_eq!(
        BlobError::io(std::io::Error::other("disk")).invalid_range_values(),
        None
    );
}

#[test]
fn test_range_error_includes_its_values() {
    assert_eq!(
        BlobError::invalid_range(3, 9, 7).to_string(),
        "range 3..9 exceeds 7 bytes"
    );
}

#[test]
fn test_limit_error_includes_its_values() {
    assert_eq!(
        BlobError::limit_exceeded(6, 7).to_string(),
        "blob size 7 exceeds 6 byte limit"
    );
}

#[test]
fn test_io_error_reports_a_source() {
    assert!(BlobError::io(std::io::Error::other("disk")).source().is_some());
}

#[test]
fn test_not_found_error_has_no_source() {
    assert!(BlobError::not_found(&Digest::of(b"missing")).source().is_none());
}
