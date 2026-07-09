use std::collections::BTreeMap;

use url::Url;

use crate::{CoreMetadata, Meta, Provenance, SimpleError, Yanked, parse_detail_html, parse_index_html};

fn base() -> Url {
    Url::parse("https://pypi.org/simple/flask/").unwrap()
}

#[test]
fn test_parse_index_html_uses_anchor_text_and_href_fallback() {
    let html = r#"<!DOCTYPE html><html><head>
        <base href="https://files.example/simple/">
        <meta name="pypi:repository-version" content="1.4">
        </head><body>
        <a href="Flask/"> Flask </a>
        <a href="zope.interface/"></a>
        <a>skip</a>
        </body></html>"#;
    let parsed = parse_index_html(html, &Url::parse("https://pypi.org/simple/").unwrap()).unwrap();
    assert_eq!(
        parsed
            .projects
            .iter()
            .map(|project| project.name.as_str())
            .collect::<Vec<_>>(),
        vec!["Flask", "zope.interface"]
    );
}

#[test]
fn test_parse_full_anchor() {
    let html = r#"<!DOCTYPE html><html><head>
        <meta name="pypi:repository-version" content="1.4">
        <meta name="pypi:project-status" content="archived">
        <meta name="pypi:project-status-reason" content="read only">
        </head><body>
        <a href="../../packages/flask-2.0-py3-none-any.whl#sha256=abc123"
           data-requires-python="&gt;=3.7" data-yanked="broken"
           data-core-metadata="sha256=deadbeef" data-gpg-sig="true"
           data-size="123" data-upload-time="2024-01-01T00:00:00Z"
           data-provenance="https://example.test/provenance">flask-2.0-py3-none-any.whl</a>
        </body></html>"#;
    let parsed = parse_detail_html("flask", html, &base()).unwrap();
    assert_eq!(parsed.name, "flask");
    assert_eq!(parsed.meta.project_status.as_deref(), Some("archived"));
    assert_eq!(parsed.meta.project_status_reason.as_deref(), Some("read only"));
    assert_eq!(parsed.files.len(), 1);
    let file = &parsed.files[0];
    assert_eq!(file.filename, "flask-2.0-py3-none-any.whl");
    assert_eq!(file.url, "https://pypi.org/packages/flask-2.0-py3-none-any.whl");
    assert_eq!(
        file.hashes,
        BTreeMap::from([("sha256".to_owned(), "abc123".to_owned())])
    );
    assert_eq!(file.requires_python.as_deref(), Some(">=3.7"));
    assert_eq!(file.size, Some(123));
    assert_eq!(file.upload_time.as_deref(), Some("2024-01-01T00:00:00Z"));
    assert_eq!(file.yanked, Yanked::Reason("broken".to_owned()));
    assert_eq!(
        file.core_metadata,
        CoreMetadata::Hashes(BTreeMap::from([("sha256".to_owned(), "deadbeef".to_owned())]))
    );
    assert_eq!(file.gpg_sig, Some(true));
    assert_eq!(
        file.provenance,
        Provenance::Url("https://example.test/provenance".to_owned())
    );
}

#[test]
fn test_parse_yanked_empty_and_core_metadata_values() {
    let html = r#"<a href="x-1.whl" data-yanked="" data-core-metadata="true">x-1.whl</a>
        <a href="x-2.whl" data-core-metadata="false">x-2.whl</a>
        <a href="x-3.whl" data-core-metadata="available">x-3.whl</a>"#;
    let file = &parse_detail_html("x", html, &base()).unwrap().files[0];
    assert_eq!(file.yanked, Yanked::Yes);
    assert_eq!(file.core_metadata, CoreMetadata::Available);
    let file = &parse_detail_html("x", html, &base()).unwrap().files[1];
    assert_eq!(file.core_metadata, CoreMetadata::Absent);
    let file = &parse_detail_html("x", html, &base()).unwrap().files[2];
    assert_eq!(file.core_metadata, CoreMetadata::Available);
}

#[test]
fn test_parse_legacy_dist_info_metadata_and_no_hash() {
    let html = r#"<a href="x-1.tar.gz" data-dist-info-metadata="sha256=aa">x-1.tar.gz</a>"#;
    let file = &parse_detail_html("x", html, &base()).unwrap().files[0];
    assert!(file.hashes.is_empty());
    assert_eq!(
        file.dist_info_metadata,
        CoreMetadata::Hashes(BTreeMap::from([("sha256".to_owned(), "aa".to_owned())]))
    );
    assert_eq!(file.core_metadata, CoreMetadata::Absent);
    assert_eq!(file.yanked, Yanked::No);
    assert!(file.requires_python.is_none());
}

#[test]
fn test_parse_ignores_irrelevant_meta_and_gpg_sig_edges() {
    let html = r#"<meta content="ignored"><meta name="other" content="ignored">
        <a href="signed.whl" data-gpg-sig>signed.whl</a>
        <a href="unknown.whl" data-gpg-sig="unknown">unknown.whl</a>"#;
    let parsed = parse_detail_html("x", html, &base()).unwrap();
    assert_eq!(parsed.meta, Meta::default());
    assert_eq!(parsed.files[0].gpg_sig, Some(true));
    assert_eq!(parsed.files[1].gpg_sig, None);
}

#[test]
fn test_anchor_without_href_is_skipped() {
    let html = "<a>not a link</a><a href=\"good-1.whl\">good-1.whl</a>";
    let parsed = parse_detail_html("good", html, &base()).unwrap();
    assert_eq!(parsed.files.len(), 1);
    assert_eq!(parsed.files[0].filename, "good-1.whl");
}

#[test]
fn test_parse_html_case_base_filename_and_encoded_hashes() {
    let html = r#"<!DOCTYPE html><HTML><HEAD>
        <BASE HREF="https://files.example/packages/">
        <META NAME="pypi:repository-version" CONTENT="1.4">
        <META NAME="pypi:project-status" CONTENT="archived">
        </HEAD><BODY>
        <A HREF="pkg-1.0%2Bcpu-py3-none-any.whl?download=1#sha256%3Dabc123"
           DATA-REQUIRES-PYTHON="&gt;=3.11">wrong name</A>
        <a href="pkg-1.0.tar%2egz#sha256%3dabc%zz">encoded</a>
        <a href="pkg-1.0.tar.gz#main">pkg-1.0.tar.gz</a>
        <a href="pkg-1.0.zip#egg=pkg&sha256=def456">pkg-1.0.zip</a>
        </BODY></HTML>"#;

    let parsed = parse_detail_html("pkg", html, &base()).unwrap();

    assert_eq!(parsed.meta.project_status.as_deref(), Some("archived"));
    assert_eq!(
        parsed
            .files
            .iter()
            .map(|file| (
                file.filename.as_str(),
                file.url.as_str(),
                file.hashes.get("sha256").map(String::as_str),
                file.requires_python.as_deref(),
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                "pkg-1.0+cpu-py3-none-any.whl",
                "https://files.example/packages/pkg-1.0%2Bcpu-py3-none-any.whl?download=1",
                Some("abc123"),
                Some(">=3.11"),
            ),
            (
                "pkg-1.0.tar.gz",
                "https://files.example/packages/pkg-1.0.tar%2egz",
                Some("abc%zz"),
                None,
            ),
            (
                "pkg-1.0.tar.gz",
                "https://files.example/packages/pkg-1.0.tar.gz",
                None,
                None,
            ),
            (
                "pkg-1.0.zip",
                "https://files.example/packages/pkg-1.0.zip",
                Some("def456"),
                None,
            ),
        ]
    );
}

#[test]
fn test_empty_or_no_anchors_yields_no_files() {
    assert!(
        parse_detail_html("x", "<html><body>nothing</body></html>", &base())
            .unwrap()
            .files
            .is_empty()
    );
}

#[test]
fn test_rejects_unsupported_major_api_version() {
    let html = r#"<meta name="pypi:repository-version" content="2.0">"#;
    let err = parse_detail_html("x", html, &base()).unwrap_err();
    assert!(matches!(err, SimpleError::UnsupportedApiVersion(version) if version == "2.0"));
}
