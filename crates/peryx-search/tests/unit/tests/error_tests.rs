use peryx_storage::meta::{MetaError, MetaScanError};

use crate::SearchError;

#[test]
fn test_bad_request_classification() {
    for (error, expected) in [
        (SearchError::InvalidSource("bad".to_owned()), true),
        (SearchError::InvalidAvailability("bad".to_owned()), true),
        (
            SearchError::Tantivy(tantivy::TantivyError::InvalidArgument("bad".to_owned())),
            true,
        ),
        (SearchError::Indexer("failed".to_owned()), false),
    ] {
        assert_eq!(error.is_bad_request(), expected, "{error}");
    }
}

#[test]
fn test_scan_visit_errors_preserve_the_visitor_error() {
    let error = SearchError::from(MetaScanError::Visit(SearchError::InvalidSource("bad".to_owned())));

    assert!(matches!(error, SearchError::InvalidSource(value) if value == "bad"));
}

#[test]
fn test_scan_store_errors_preserve_the_store_error() {
    let error = SearchError::from(MetaScanError::<SearchError>::Store(MetaError::DriverPrecondition(
        "failed".to_owned(),
    )));

    assert!(matches!(error, SearchError::Meta(MetaError::DriverPrecondition(value)) if value == "failed"));
}
