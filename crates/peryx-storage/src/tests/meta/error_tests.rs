use std::error::Error as _;

use crate::meta::{MetaError, MetaScanError};

#[test]
fn test_scan_store_error_reports_source() {
    let decode = serde_json::from_slice::<serde_json::Value>(b"not json").unwrap_err();
    let err: MetaScanError<std::io::Error> = MetaError::Decode(decode).into();
    assert!(!err.to_string().is_empty());
    assert!(err.source().is_some());
}
