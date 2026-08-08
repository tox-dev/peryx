use crate::{AvailabilityFilter, PackageSource, SearchError, SearchParams, SourceFilter, truncate_to_chars};

#[test]
fn test_query_parameters_parse_all_fields() {
    assert_eq!(
        SearchParams::from_query(Some(
            "q=demo+package&route=private&type=uploaded&availability=local&page=2&page_size=50&ignored=value",
        ))
        .unwrap(),
        SearchParams {
            query: "demo package".to_owned(),
            route: Some("private".to_owned()),
            source: SourceFilter::Uploaded,
            availability: AvailabilityFilter::Local,
            page: 2,
            page_size: 50,
        }
    );
}

#[test]
fn test_query_parameters_normalize_empty_and_invalid_values() {
    for (query, expected) in [
        (
            "route=&type=&availability=&page=0&page_size=12",
            SearchParams::default(),
        ),
        (
            "type=all&availability=all&page=bad&page_size=bad",
            SearchParams::default(),
        ),
    ] {
        assert_eq!(SearchParams::from_query(Some(query)).unwrap(), expected, "{query}");
    }
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
        ("uploaded", SourceFilter::Uploaded, Some(PackageSource::Uploaded)),
        ("cached", SourceFilter::Cached, Some(PackageSource::Cached)),
        ("override", SourceFilter::Override, Some(PackageSource::Override)),
    ] {
        assert_eq!(SourceFilter::from_value(value), Some(filter), "{value}");
        assert_eq!(filter.as_str(), value, "{value}");
        assert_eq!(filter.package_source(), source, "{value}");
    }
    assert_eq!(SourceFilter::from_value("blocked"), None);
}

#[test]
fn test_package_source_values_round_trip() {
    for (value, source, label) in [
        ("uploaded", PackageSource::Uploaded, "Uploaded"),
        ("cached", PackageSource::Cached, "Cached"),
        ("override", PackageSource::Override, "Override"),
    ] {
        assert_eq!(PackageSource::from_value(value), Some(source), "{value}");
        assert_eq!(source.as_str(), value, "{value}");
        assert_eq!(source.label(), label, "{value}");
    }
    assert_eq!(PackageSource::from_value("blocked"), None);
}

#[test]
fn test_truncate_to_chars_preserves_utf8_boundaries() {
    assert_eq!(truncate_to_chars("éx", 3), "éx");
    assert_eq!(truncate_to_chars("éx", 2), "é");
    assert_eq!(truncate_to_chars("éx", 1), "");
}
