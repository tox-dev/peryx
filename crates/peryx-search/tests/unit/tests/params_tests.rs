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
            pattern_authority: false,
        }
    );
}

#[rstest]
#[case::substring("q=widget", false)]
#[case::pattern("q=re%3Awidget", true)]
#[case::padded_pattern("q=+re%3Awidget+", true)]
fn test_query_parameters_recognize_the_pattern_dialect(#[case] query: &str, #[case] expected: bool) {
    assert_eq!(SearchParams::from_query(Some(query)).unwrap().is_pattern(), expected);
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

#[rstest]
#[case::last_window(100, 100, Some(9_900))]
#[case::first_window_above_limit(101, 100, None)]
#[case::offset_overflow(usize::MAX, 100, None)]
#[case::end_overflow(2, usize::MAX, None)]
fn test_query_offset_bounds_the_result_window(
    #[case] page: usize,
    #[case] page_size: usize,
    #[case] expected: Option<usize>,
) {
    let params = SearchParams {
        page,
        page_size,
        ..SearchParams::default()
    };

    match expected {
        Some(offset) => assert_eq!(params.offset().unwrap(), offset),
        None => assert!(matches!(
            params.offset(),
            Err(SearchError::ResultWindowTooLarge {
                page: rejected_page,
                page_size: rejected_page_size,
                max: 10_000,
            }) if (rejected_page, rejected_page_size) == (page, page_size)
        )),
    }
}

#[rstest]
#[case::first_window_above_limit("page=101&page_size=100")]
#[case::arithmetic_overflow(&format!("page={}&page_size=100", usize::MAX))]
fn test_query_parameters_reject_oversized_result_windows(#[case] query: &str) {
    assert!(matches!(
        SearchParams::from_query(Some(query)),
        Err(SearchError::ResultWindowTooLarge { .. })
    ));
}

#[test]
fn test_query_parameters_accept_the_maximum_result_window() {
    let params = SearchParams::from_query(Some("page=100&page_size=100")).unwrap();

    assert_eq!(
        (params.page, params.page_size, params.offset().unwrap()),
        (100, 100, 9_900)
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
fn test_availability_filter_values_round_trip() {
    for (value, filter) in [("all", AvailabilityFilter::All), ("local", AvailabilityFilter::Local)] {
        assert_eq!(
            (AvailabilityFilter::from_value(value), filter.as_str()),
            (Some(filter), value)
        );
    }
    assert_eq!(AvailabilityFilter::from_value("blocked"), None);
}

#[test]
fn test_truncate_to_chars_preserves_utf8_boundaries() {
    assert_eq!(truncate_to_chars("éx", 3), "éx");
    assert_eq!(truncate_to_chars("éx", 2), "é");
    assert_eq!(truncate_to_chars("éx", 1), "");
}
