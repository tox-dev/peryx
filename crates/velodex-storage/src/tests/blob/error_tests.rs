use std::error::Error as _;

use crate::blob::{BlobError, BlobScanError};

#[test]
fn test_scan_store_error_reports_source() {
    let err: BlobScanError<std::io::Error> = BlobError::NotFound("missing".to_owned()).into();
    assert_eq!(err.to_string(), "blob missing not found");
    assert!(err.source().is_some());
}
