use super::{RangeRequest, parse_range};

#[test]
fn test_parse_range() {
    for (case, header, blob_size, expected) in [
        ("no header", None, 10, RangeRequest::Whole),
        ("wrong unit", Some("items=0-1"), 10, RangeRequest::Whole),
        ("multiple ranges", Some("bytes=0-1,4-5"), 10, RangeRequest::Whole),
        ("missing separator", Some("bytes=abc"), 10, RangeRequest::Whole),
        ("missing bounds", Some("bytes=-"), 10, RangeRequest::Whole),
        ("non-numeric bounds", Some("bytes=a-b"), 10, RangeRequest::Whole),
        ("non-numeric start", Some("bytes=x-"), 10, RangeRequest::Whole),
        ("non-numeric suffix", Some("bytes=-x"), 10, RangeRequest::Whole),
        ("non-numeric end", Some("bytes=1-abc"), 10, RangeRequest::Whole),
        (
            "non-numeric start with end",
            Some("bytes=abc-9"),
            10,
            RangeRequest::Whole,
        ),
        ("closed span", Some("bytes=2-5"), 10, RangeRequest::Partial(2..6)),
        ("overshooting end", Some("bytes=2-99"), 10, RangeRequest::Partial(2..10)),
        ("open end", Some("bytes=4-"), 10, RangeRequest::Partial(4..10)),
        ("suffix", Some("bytes=-3"), 10, RangeRequest::Partial(7..10)),
        ("oversized suffix", Some("bytes=-99"), 10, RangeRequest::Partial(0..10)),
        ("whitespace", Some("bytes= 2 - 5 "), 10, RangeRequest::Partial(2..6)),
        ("reversed", Some("bytes=5-2"), 10, RangeRequest::Whole),
        ("start at end", Some("bytes=10-12"), 10, RangeRequest::Unsatisfiable),
        ("open start at end", Some("bytes=10-"), 10, RangeRequest::Unsatisfiable),
        ("empty suffix", Some("bytes=-0"), 10, RangeRequest::Unsatisfiable),
        ("closed over empty", Some("bytes=0-0"), 0, RangeRequest::Unsatisfiable),
        ("open over empty", Some("bytes=0-"), 0, RangeRequest::Unsatisfiable),
        ("suffix over empty", Some("bytes=-1"), 0, RangeRequest::Unsatisfiable),
    ] {
        assert_eq!(parse_range(header, blob_size), expected, "{case}");
    }
}
