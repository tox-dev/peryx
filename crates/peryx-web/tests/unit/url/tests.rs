use rstest::rstest;

use super::{
    browse_index_url, search_api_url, search_page_url, stats_api_url, stats_index_url, stats_resource_url,
    ui_browse_url,
};

#[test]
fn browse_index_url_encodes_index() {
    assert_eq!(browse_index_url("root/alpha"), "/browse?index=root%2Falpha");
}

#[test]
fn search_page_url_encodes_non_default_arguments() {
    for (query, source_type, availability, page, page_size, expected) in [
        ("", "", "", 1, 25, "/search?page_size=25"),
        (
            "cached record",
            "override",
            "local",
            2,
            50,
            "/search?q=cached%20record&type=override&availability=local&page=2&page_size=50",
        ),
    ] {
        assert_eq!(
            search_page_url(query, source_type, availability, page, page_size),
            expected,
            "{query}"
        );
    }
}

#[test]
fn stats_index_url_encodes_index() {
    assert_eq!(stats_index_url("root/alpha"), "/stats?index=root%2Falpha");
}

#[test]
fn stats_resource_url_encodes_resource() {
    assert_eq!(
        stats_resource_url("root/alpha", "cached record"),
        "/stats?index=root%2Falpha&resource=cached%20record"
    );
}

#[rstest]
#[case::without_query("", "/+ui/browse")]
#[case::with_query("index=root%2Falpha&page=2", "/+ui/browse?index=root%2Falpha&page=2")]
fn ui_browse_url_appends_raw_query(#[case] raw_query: &str, #[case] expected: &str) {
    assert_eq!(ui_browse_url(raw_query), expected);
}

#[rstest]
#[case::defaults("", "", "", 1, 25, "/+search?page_size=25")]
#[case::all_filters("", "all", "all", 1, 25, "/+search?page_size=25")]
#[case::every_argument(
    "cached record",
    "override",
    "local",
    2,
    50,
    "/+search?q=cached%20record&type=override&availability=local&page=2&page_size=50"
)]
fn search_api_url_encodes_non_default_arguments(
    #[case] query: &str,
    #[case] source_type: &str,
    #[case] availability: &str,
    #[case] page: usize,
    #[case] page_size: usize,
    #[case] expected: &str,
) {
    assert_eq!(
        search_api_url(query, source_type, availability, page, page_size),
        expected
    );
}

#[rstest]
#[case::routes(None, None, "/+stats")]
#[case::resource_without_index(None, Some("cached record"), "/+stats")]
#[case::index(Some("root/alpha"), None, "/+stats?index=root%2Falpha")]
#[case::resource(
    Some("root/alpha"),
    Some("cached record"),
    "/+stats?index=root%2Falpha&resource=cached%20record"
)]
fn stats_api_url_encodes_selected_depth(
    #[case] route: Option<&str>,
    #[case] resource: Option<&str>,
    #[case] expected: &str,
) {
    assert_eq!(stats_api_url(route, resource), expected);
}
