//! Matching an `If-None-Match` field against an entity tag.

use rstest::rstest;

use crate::conditional::if_none_match;

const ETAG: &str = "\"9f86d081\"";

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
