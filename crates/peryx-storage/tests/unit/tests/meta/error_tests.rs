use std::error::Error as _;

use crate::meta::{MetaError, MetaScanError, MetaStore};

#[test]
fn test_scan_store_error_reports_source() {
    let decode = serde_json::from_slice::<serde_json::Value>(b"not json").unwrap_err();
    let err: MetaScanError<std::io::Error> = MetaError::Decode(decode).into();
    assert!(!err.to_string().is_empty());
    assert!(err.source().is_some());
}

#[test]
fn test_database_already_open_error_is_classified() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("meta.redb");
    let _store = MetaStore::open(&path).unwrap();
    assert!(MetaStore::open(path).unwrap_err().is_database_already_open());
}

#[test]
fn test_scan_visit_error_reports_visitor_display_and_source() {
    let error = MetaScanError::Visit(std::io::Error::other("visitor stopped"));
    assert_eq!(error.to_string(), "visitor stopped");
    assert_eq!(error.source().unwrap().to_string(), "visitor stopped");
}
