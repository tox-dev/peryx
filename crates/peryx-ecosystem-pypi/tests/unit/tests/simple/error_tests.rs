use std::error::Error as _;

use crate::SimpleError;

#[test]
fn test_simple_error_json_source() {
    let err = crate::parse_detail(b"not json").unwrap_err();
    assert!(matches!(err, SimpleError::Json(_)));
    assert!(err.source().is_some());
    assert!(err.to_string().contains("expected"));
}

#[test]
fn test_simple_error_html_source() {
    let err = SimpleError::from(tl::ParseError::InvalidLength);
    assert!(matches!(err, SimpleError::Html(tl::ParseError::InvalidLength)));
    assert!(err.source().is_some());
    assert_eq!(
        err.to_string(),
        "invalid upstream Simple API HTML: The input string length is too large to fit in a `u32`"
    );
}
