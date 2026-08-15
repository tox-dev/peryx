use super::{browse_index_url, search_page_url, stats_index_url, stats_resource_url};

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
