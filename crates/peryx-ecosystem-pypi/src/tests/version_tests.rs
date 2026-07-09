use crate::{parse_version, sorted_desc};

#[test]
fn test_parse_version_valid_and_invalid() {
    assert!(parse_version("1.2.3").is_some());
    assert!(parse_version("2024.1a1").is_some());
    assert!(parse_version("not-a-version").is_none());
}

#[test]
fn test_parse_version_orders_pre_and_post() {
    assert!(parse_version("1.0a1") < parse_version("1.0"));
    assert!(parse_version("1.0") < parse_version("1.0.post1"));
}

#[test]
fn test_sorted_desc_newest_first() {
    let input = vec!["1.0".to_owned(), "2.0".to_owned(), "1.5".to_owned(), "1.0a1".to_owned()];
    assert_eq!(sorted_desc(&input), vec!["2.0", "1.5", "1.0", "1.0a1"]);
}

#[test]
fn test_sorted_desc_unparseable_sort_after() {
    let input = vec!["1.0".to_owned(), "legacy".to_owned(), "2.0".to_owned()];
    let sorted = sorted_desc(&input);
    assert_eq!(sorted[0], "2.0");
    assert_eq!(sorted[1], "1.0");
    assert_eq!(sorted[2], "legacy");
}

#[test]
fn test_sorted_desc_two_unparseable_keep_order() {
    let input = vec!["alpha".to_owned(), "beta".to_owned()];
    assert_eq!(sorted_desc(&input), vec!["alpha", "beta"]);
}
