use std::collections::BTreeMap;

use url::Url;

use crate::pypi::parse_detail_html;
use crate::pypi::{CoreMetadata, Yanked};

fn base() -> Url {
    Url::parse("https://pypi.org/simple/flask/").unwrap()
}

#[test]
fn test_parse_full_anchor() {
    let html = r#"<!DOCTYPE html><html><body>
        <a href="../../packages/flask-2.0-py3-none-any.whl#sha256=abc123"
           data-requires-python="&gt;=3.7" data-yanked="broken"
           data-core-metadata="sha256=deadbeef">flask-2.0-py3-none-any.whl</a>
        </body></html>"#;
    let parsed = parse_detail_html("flask", html, &base());
    assert_eq!(parsed.name, "flask");
    assert_eq!(parsed.files.len(), 1);
    let file = &parsed.files[0];
    assert_eq!(file.filename, "flask-2.0-py3-none-any.whl");
    assert_eq!(file.url, "https://pypi.org/packages/flask-2.0-py3-none-any.whl");
    assert_eq!(
        file.hashes,
        BTreeMap::from([("sha256".to_owned(), "abc123".to_owned())])
    );
    assert_eq!(file.requires_python.as_deref(), Some(">=3.7"));
    assert_eq!(file.yanked, Yanked::Reason("broken".to_owned()));
    assert_eq!(
        file.core_metadata,
        CoreMetadata::Hashes(BTreeMap::from([("sha256".to_owned(), "deadbeef".to_owned())]))
    );
}

#[test]
fn test_parse_yanked_empty_and_core_metadata_true() {
    let html = r#"<a href="x-1.whl" data-yanked="" data-core-metadata="true">x-1.whl</a>"#;
    let file = &parse_detail_html("x", html, &base()).files[0];
    assert_eq!(file.yanked, Yanked::Yes);
    assert_eq!(file.core_metadata, CoreMetadata::Available);
}

#[test]
fn test_parse_legacy_dist_info_metadata_and_no_hash() {
    let html = r#"<a href="x-1.tar.gz" data-dist-info-metadata="sha256=aa">x-1.tar.gz</a>"#;
    let file = &parse_detail_html("x", html, &base()).files[0];
    assert!(file.hashes.is_empty());
    assert_eq!(
        file.core_metadata,
        CoreMetadata::Hashes(BTreeMap::from([("sha256".to_owned(), "aa".to_owned())]))
    );
    assert_eq!(file.yanked, Yanked::No);
    assert!(file.requires_python.is_none());
}

#[test]
fn test_anchor_without_href_is_skipped() {
    let html = "<a>not a link</a><a href=\"good-1.whl\">good-1.whl</a>";
    let parsed = parse_detail_html("good", html, &base());
    assert_eq!(parsed.files.len(), 1);
    assert_eq!(parsed.files[0].filename, "good-1.whl");
}

#[test]
fn test_empty_or_no_anchors_yields_no_files() {
    assert!(
        parse_detail_html("x", "<html><body>nothing</body></html>", &base())
            .files
            .is_empty()
    );
}
