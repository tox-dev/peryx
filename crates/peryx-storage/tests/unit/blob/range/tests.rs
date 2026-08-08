use super::{RangeRequest, parse_range};

#[test]
fn test_parse_range_serves_the_whole_blob_when_no_range_applies() {
    for header in [
        None,
        Some("items=0-1"),
        Some("bytes=0-1,4-5"),
        Some("bytes=abc"),
        Some("bytes=-"),
        Some("bytes=a-b"),
        Some("bytes=x-"),
        Some("bytes=-x"),
        Some("bytes=1-abc"),
        Some("bytes=abc-9"),
    ] {
        assert_eq!(parse_range(header, 10), RangeRequest::Whole, "{header:?}");
    }
}

#[test]
fn test_parse_range_reads_a_closed_span_end_exclusive() {
    assert_eq!(parse_range(Some("bytes=2-5"), 10), RangeRequest::Partial(2..6));
}

#[test]
fn test_parse_range_clamps_an_overshooting_end_to_the_blob() {
    assert_eq!(parse_range(Some("bytes=2-99"), 10), RangeRequest::Partial(2..10));
}

#[test]
fn test_parse_range_reads_from_an_offset_to_the_end() {
    assert_eq!(parse_range(Some("bytes=4-"), 10), RangeRequest::Partial(4..10));
}

#[test]
fn test_parse_range_reads_a_suffix() {
    assert_eq!(parse_range(Some("bytes=-3"), 10), RangeRequest::Partial(7..10));
}

#[test]
fn test_parse_range_uses_the_whole_blob_for_a_suffix_past_the_size() {
    assert_eq!(parse_range(Some("bytes=-99"), 10), RangeRequest::Partial(0..10));
}

#[test]
fn test_parse_range_ignores_leading_and_trailing_whitespace() {
    assert_eq!(parse_range(Some("bytes= 2 - 5 "), 10), RangeRequest::Partial(2..6));
}

#[test]
fn test_parse_range_serves_the_whole_blob_for_a_reversed_range() {
    assert_eq!(parse_range(Some("bytes=5-2"), 10), RangeRequest::Whole);
}

#[test]
fn test_parse_range_rejects_a_first_byte_at_or_past_the_end() {
    assert_eq!(parse_range(Some("bytes=10-12"), 10), RangeRequest::Unsatisfiable);
    assert_eq!(parse_range(Some("bytes=10-"), 10), RangeRequest::Unsatisfiable);
}

#[test]
fn test_parse_range_rejects_a_zero_length_suffix() {
    assert_eq!(parse_range(Some("bytes=-0"), 10), RangeRequest::Unsatisfiable);
}

#[test]
fn test_parse_range_rejects_any_well_formed_range_over_an_empty_blob() {
    for header in ["bytes=0-0", "bytes=0-", "bytes=-1"] {
        assert_eq!(parse_range(Some(header), 0), RangeRequest::Unsatisfiable, "{header}");
    }
}
