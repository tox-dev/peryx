use std::time::{Duration, UNIX_EPOCH};

use axum::http::{HeaderMap, HeaderValue, header};
use rstest::rstest;

use crate::conditional::{applicable_range, http_date, if_modified_since, if_none_match, last_modified};

const ETAG: &str = "\"9f86d081\"";

const WROTE_AT: Duration = Duration::from_secs(1_792_567_711);
const AFTER: Duration = Duration::from_secs(1_792_567_712);

#[rstest]
#[case::exact("\"9f86d081\"")]
#[case::weak("W/\"9f86d081\"")]
#[case::any("*")]
#[case::list("\"other\", \"9f86d081\"")]
#[case::list_unspaced("\"other\",W/\"9f86d081\"")]
fn test_if_none_match_names_the_representation(#[case] field: &str) {
    assert!(if_none_match(field, ETAG), "{field}");
}

#[rstest]
#[case::other_tag("\"other\"")]
#[case::unquoted("9f86d081")]
#[case::prefix("\"9f86d081x\"")]
#[case::empty("")]
#[case::malformed("W/*")]
fn test_if_none_match_leaves_the_full_response(#[case] field: &str) {
    assert!(!if_none_match(field, ETAG), "{field}");
}

fn headers(fields: &[(&str, &str)]) -> HeaderMap {
    fields
        .iter()
        .map(|(name, value)| {
            (
                name.parse().expect("a test header name"),
                value.parse().expect("a test header value"),
            )
        })
        .collect()
}

#[rstest]
#[case::no_condition(&[("range", "bytes=0-3")])]
#[case::matching_tag(&[("range", "bytes=0-3"), ("if-range", ETAG)])]
fn test_applicable_range_serves_the_range(#[case] fields: &[(&str, &str)]) {
    assert_eq!(applicable_range(&headers(fields), ETAG), Some("bytes=0-3"));
}

#[rstest]
#[case::stale_tag("\"other\"")]
#[case::weak_tag("W/\"9f86d081\"")]
#[case::date("Wed, 21 Oct 2015 07:28:00 GMT")]
#[case::malformed("9f86d081")]
#[case::any("*")]
fn test_applicable_range_drops_the_range_a_stale_if_range_asks_for(#[case] field: &str) {
    let fields = [("range", "bytes=0-3"), ("if-range", field)];
    assert_eq!(applicable_range(&headers(&fields), ETAG), None, "{field}");
}

#[test]
fn test_applicable_range_ignores_an_if_range_without_a_range() {
    assert_eq!(applicable_range(&headers(&[("if-range", "\"other\"")]), ETAG), None);
}

#[test]
fn test_applicable_range_drops_repeated_range_lines_as_a_multi_range() {
    let mut map = HeaderMap::new();
    map.append(header::RANGE, HeaderValue::from_static("bytes=0-1"));
    map.append(header::RANGE, HeaderValue::from_static("bytes=4-5"));

    assert_eq!(applicable_range(&map, ETAG), None);
}

#[test]
fn test_applicable_range_drops_the_range_when_if_range_repeats() {
    let mut map = HeaderMap::new();
    map.append(header::RANGE, HeaderValue::from_static("bytes=0-3"));
    map.append(header::IF_RANGE, HeaderValue::from_str(ETAG).unwrap());
    map.append(header::IF_RANGE, HeaderValue::from_str(ETAG).unwrap());

    assert_eq!(applicable_range(&map, ETAG), None);
}

#[test]
fn test_last_modified_is_the_write_as_a_whole_second() {
    let stored = UNIX_EPOCH + WROTE_AT + Duration::from_millis(900);

    let modified = last_modified(stored, UNIX_EPOCH + AFTER);

    assert_eq!(modified, UNIX_EPOCH + WROTE_AT);
    assert_eq!(http_date(modified), "Wed, 21 Oct 2026 07:28:31 GMT");
}

#[test]
fn test_last_modified_clamps_a_write_dated_after_the_response() {
    let stored = UNIX_EPOCH + AFTER + Duration::from_hours(24);

    assert_eq!(last_modified(stored, UNIX_EPOCH + WROTE_AT), UNIX_EPOCH + WROTE_AT);
}

#[test]
fn test_a_truncated_date_the_client_echoes_back_still_matches() {
    let stored = UNIX_EPOCH + WROTE_AT + Duration::from_millis(900);
    let modified = last_modified(stored, UNIX_EPOCH + AFTER + Duration::from_hours(24));

    assert!(if_modified_since(&http_date(modified), modified));
}

#[rstest]
#[case::same_second("Wed, 21 Oct 2026 07:28:31 GMT")]
#[case::later("Thu, 22 Oct 2026 07:28:31 GMT")]
#[case::rfc850("Wednesday, 21-Oct-26 07:28:31 GMT")]
#[case::asctime("Wed Oct 21 07:28:31 2026")]
fn test_if_modified_since_covers_the_stored_copy(#[case] field: &str) {
    assert!(if_modified_since(field, UNIX_EPOCH + WROTE_AT), "{field}");
}

#[rstest]
#[case::a_second_early("Wed, 21 Oct 2026 07:28:30 GMT")]
#[case::stale("Tue, 15 Nov 1994 08:12:31 GMT")]
#[case::not_a_date("last tuesday")]
#[case::empty("")]
#[case::two_dates("Wed, 21 Oct 2026 07:28:31 GMT, Thu, 22 Oct 2026 07:28:31 GMT")]
fn test_if_modified_since_leaves_the_full_response(#[case] field: &str) {
    assert!(!if_modified_since(field, UNIX_EPOCH + WROTE_AT), "{field}");
}
