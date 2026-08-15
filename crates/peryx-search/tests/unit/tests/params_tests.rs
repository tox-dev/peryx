use crate::{AvailabilityFilter, ContentSource, SearchError, SearchParams, SourceFilter, truncate_to_chars};
use rstest::rstest;

#[test]
fn test_query_parameters_parse_all_fields() {
    assert_eq!(
        SearchParams::from_query(Some(
            "q=demo+resource&route=private&type=uploaded&availability=local&page=2&page_size=50&ignored=value",
        ))
        .unwrap(),
        SearchParams {
            query: "demo resource".to_owned(),
            route: Some("private".to_owned()),
            source: SourceFilter::Uploaded,
            availability: AvailabilityFilter::Local,
            page: 2,
            page_size: 50,
        }
    );
}

#[rstest]
#[case::empty("route=&type=&availability=&page=0&page_size=12")]
#[case::invalid("type=all&availability=all&page=bad&page_size=bad")]
fn test_query_parameters_normalize_empty_and_invalid_values(#[case] query: &str) {
    assert_eq!(SearchParams::from_query(Some(query)).unwrap(), SearchParams::default());
}

#[test]
fn test_query_parameters_reject_unknown_source_filters() {
    let error = SearchParams::from_query(Some("type=blocked")).unwrap_err();

    assert!(matches!(error, SearchError::InvalidSource(value) if value == "blocked"));
}

#[test]
fn test_query_offset_saturates() {
    assert_eq!(
        SearchParams {
            page: usize::MAX,
            page_size: 100,
            ..SearchParams::default()
        }
        .offset(),
        usize::MAX
    );
}

#[test]
fn test_source_filter_values_round_trip() {
    for (value, filter, source) in [
        ("all", SourceFilter::All, None),
        ("uploaded", SourceFilter::Uploaded, Some(ContentSource::Uploaded)),
        ("cached", SourceFilter::Cached, Some(ContentSource::Cached)),
        ("override", SourceFilter::Override, Some(ContentSource::Override)),
    ] {
        assert_eq!(
            (
                SourceFilter::from_value(value),
                filter.as_str(),
                filter.content_source()
            ),
            (Some(filter), value, source),
            "{value}"
        );
    }
    assert_eq!(SourceFilter::from_value("blocked"), None);
}

#[test]
fn test_content_source_values_round_trip() {
    for (value, source, label) in [
        ("uploaded", ContentSource::Uploaded, "Uploaded"),
        ("cached", ContentSource::Cached, "Cached"),
        ("override", ContentSource::Override, "Override"),
    ] {
        assert_eq!(
            (ContentSource::from_value(value), source.as_str(), source.label()),
            (Some(source), value, label),
            "{value}"
        );
    }
    assert_eq!(ContentSource::from_value("blocked"), None);
}

#[test]
fn test_truncate_to_chars_preserves_utf8_boundaries() {
    assert_eq!(truncate_to_chars("éx", 3), "éx");
    assert_eq!(truncate_to_chars("éx", 2), "é");
    assert_eq!(truncate_to_chars("éx", 1), "");
}
