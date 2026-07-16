use std::error::Error as _;

use crate::blob::{BlobError, BlobOperation, BlobScanError};

#[test]
fn test_blob_operations_use_backend_verbs() {
    for (operation, expected) in [
        (BlobOperation::Put, "put"),
        (BlobOperation::Get, "get"),
        (BlobOperation::Head, "head"),
        (BlobOperation::Range, "range"),
        (BlobOperation::Delete, "delete"),
        (BlobOperation::Verify, "verify"),
    ] {
        assert_eq!(operation.to_string(), expected);
    }
}

#[test]
fn test_scan_store_error_reports_source() {
    let err: BlobScanError<std::io::Error> = BlobError::NotFound("missing".to_owned()).into();
    assert_eq!(err.to_string(), "blob missing not found");
    assert!(err.source().is_some());
}
