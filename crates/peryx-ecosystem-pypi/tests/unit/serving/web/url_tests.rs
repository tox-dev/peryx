use super::{BrowseQuery, archive_url};

#[test]
fn archive_url_links_selections() {
    let query = BrowseQuery::parse("index=hosted&container=inner.zip").unwrap();
    for (selected, expected) in [
        (
            None,
            "/browse?index=hosted&project=demo&sha256=abc&file=demo-1.0-py3-none-any.whl&container=inner.zip",
        ),
        (
            Some(("nested.zip", true)),
            "/browse?index=hosted&project=demo&sha256=abc&file=demo-1.0-py3-none-any.whl&container=inner.zip&container=nested.zip",
        ),
        (
            Some(("README.txt", false)),
            "/browse?index=hosted&project=demo&sha256=abc&file=demo-1.0-py3-none-any.whl&container=inner.zip&member=README.txt",
        ),
    ] {
        assert_eq!(
            archive_url(&query, "demo", "abc", "demo-1.0-py3-none-any.whl", selected, None),
            expected
        );
    }
}
