use super::{BrowseQuery, archive_url};

#[rstest::rstest]
#[case::project("project", "demo", |q: &BrowseQuery| q.project.is_some())]
#[case::version("version", "1.0", |q: &BrowseQuery| q.version.is_some())]
#[case::filename("filename", "pkg.whl", |q: &BrowseQuery| q.filename.is_some())]
#[case::filename_match("filename_match", "regex", |q: &BrowseQuery| q.filename_match.is_some())]
#[case::sha256("sha256", "abc", |q: &BrowseQuery| q.sha256.is_some())]
#[case::file("file", "pkg.whl", |q: &BrowseQuery| q.file.is_some())]
#[case::container("container", "inner.zip", |q: &BrowseQuery| !q.containers.is_empty())]
#[case::member("member", "README.txt", |q: &BrowseQuery| q.member.is_some())]
#[case::offset("offset", "5", |q: &BrowseQuery| q.offset != 0)]
fn browse_query_parse_ignores_an_empty_value_but_keeps_a_real_one(
    #[case] key: &str,
    #[case] value: &str,
    #[case] set: fn(&BrowseQuery) -> bool,
) {
    let empty = BrowseQuery::parse(&format!("index=hosted&{key}=")).unwrap();
    assert!(!set(&empty), "{key} with an empty value must not be recorded");

    let filled = BrowseQuery::parse(&format!("index=hosted&{key}={value}")).unwrap();
    assert!(set(&filled), "{key} with a real value must be recorded");
}

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
