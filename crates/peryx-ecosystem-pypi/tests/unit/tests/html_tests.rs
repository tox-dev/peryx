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
        <a href="../../packages/flask-2.0-py3-none-any.whl#sha256=9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
           data-requires-python="&gt;=3.7" data-yanked="broken"
           data-core-metadata="sha256=2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae" data-gpg-sig="true"
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
        BTreeMap::from([(
            "sha256".to_owned(),
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08".to_owned()
        )])
    );
    assert_eq!(file.requires_python.as_deref(), Some(">=3.7"));
    assert_eq!(file.size, Some(123));
    assert_eq!(file.upload_time.as_deref(), Some("2024-01-01T00:00:00Z"));
    assert_eq!(file.yanked, Yanked::Reason("broken".to_owned()));
    assert_eq!(
        file.core_metadata,
        CoreMetadata::Hashes(BTreeMap::from([(
            "sha256".to_owned(),
            "2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae".to_owned()
        )]))
    );
    assert_eq!(file.gpg_sig, Some(true));
    assert_eq!(
        file.provenance,
        Provenance::Url("https://example.test/provenance".to_owned())
    );
}

#[test]
fn test_parse_anchor_drops_an_insecure_provenance_url() {
    let html = r#"<a href="pkg-1.0.whl" data-provenance="http://example.test/pkg.provenance">pkg-1.0.whl</a>"#;

    let file = &parse_detail_html("pkg", html, &base()).unwrap().files[0];

    assert_eq!(file.provenance, Provenance::Absent);
}

#[test]
fn test_fragment_keeps_every_supported_hash_including_sha256() {
    let html = r#"<a href="pkg-1.0.whl#md5=deadbeef&sha256=9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08">pkg-1.0.whl</a>"#;
    let file = &parse_detail_html("pkg", html, &base()).unwrap().files[0];
    assert_eq!(
        file.hashes,
        BTreeMap::from([
            ("md5".to_owned(), "deadbeef".to_owned()),
            (
                "sha256".to_owned(),
                "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08".to_owned()
            ),
        ])
    );
}

#[test]
fn test_fragment_surfaces_a_non_sha256_only_hash() {
    let html = r#"<a href="pkg-1.0.whl#md5=deadbeef">pkg-1.0.whl</a>"#;
    let file = &parse_detail_html("pkg", html, &base()).unwrap().files[0];
    assert_eq!(file.hashes, BTreeMap::from([("md5".to_owned(), "deadbeef".to_owned())]));
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
    let html = r#"<a href="x-1.tar.gz" data-dist-info-metadata="sha256=fcde2b2edba56bf408601fb721fe9b5c338d10ee429ea04fae5511b68fbf8fb9">x-1.tar.gz</a>"#;
    let file = &parse_detail_html("x", html, &base()).unwrap().files[0];
    assert!(file.hashes.is_empty());
    assert_eq!(
        file.dist_info_metadata,
        CoreMetadata::Hashes(BTreeMap::from([(
            "sha256".to_owned(),
            "fcde2b2edba56bf408601fb721fe9b5c338d10ee429ea04fae5511b68fbf8fb9".to_owned()
        )]))
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
    // A page that advertises no repository-version promises no PEP 700 fields, so it maps to the base.
    assert_eq!(
        parsed.meta,
        Meta {
            api_version: crate::API_VERSION_BASE,
            ..Meta::default()
        }
    );
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
        <A HREF="pkg-1.0%2Bcpu-py3-none-any.whl?download=1#sha256%3D9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
           DATA-REQUIRES-PYTHON="&gt;=3.11">wrong name</A>
        <a href="pkg-1.0.tar%2egz#md5%3dabc%zz">encoded</a>
        <a href="pkg-1.0.tar.gz#main">pkg-1.0.tar.gz</a>
        <a href="pkg-1.0.zip#egg=pkg&sha256=fcde2b2edba56bf408601fb721fe9b5c338d10ee429ea04fae5511b68fbf8fb9">pkg-1.0.zip</a>
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
                Some("9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"),
                Some(">=3.11"),
            ),
            (
                "pkg-1.0.tar.gz",
                "https://files.example/packages/pkg-1.0.tar%2egz",
                None,
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
                Some("fcde2b2edba56bf408601fb721fe9b5c338d10ee429ea04fae5511b68fbf8fb9"),
                None,
            ),
        ]
    );
    assert_eq!(
        parsed.files[1].hashes,
        BTreeMap::from([("md5".to_owned(), "abc%zz".to_owned())])
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

#[test]
fn test_parse_html_decodes_named_and_numeric_character_references() {
    let html = r#"<!DOCTYPE html><html><body>
        <a href="a/">A&#46;B</a>
        <a href="b/">A&#x2e;B</a>
        <a href="c/">A&#X2E;B</a>
        <a href="d/">a&amp;b</a>
        <a href="e/">a&apos;b</a>
        <a href="f/">a&lt;b&gt;c</a>
        <a href="g/">a&quot;b</a>
        <a href="h/">a&#39;b</a>
        </body></html>"#;
    let parsed = parse_index_html(html, &Url::parse("https://pypi.org/simple/").unwrap()).unwrap();
    assert_eq!(
        parsed
            .projects
            .iter()
            .map(|project| project.name.as_str())
            .collect::<Vec<_>>(),
        vec!["A.B", "A.B", "A.B", "a&b", "a'b", "a<b>c", "a\"b", "a'b"],
    );
}

#[test]
fn test_parse_html_leaves_non_references_literal() {
    let html = r#"<!DOCTYPE html><html><body>
        <a href="a/">z&#zz;z</a>
        <a href="b/">z&#xzz;z</a>
        <a href="c/">z&bogus;z</a>
        <a href="d/">z & z</a>
        <a href="e/">trailing&</a>
        </body></html>"#;
    let parsed = parse_index_html(html, &Url::parse("https://pypi.org/simple/").unwrap()).unwrap();
    assert_eq!(
        parsed
            .projects
            .iter()
            .map(|project| project.name.as_str())
            .collect::<Vec<_>>(),
        vec!["z&#zz;z", "z&#xzz;z", "z&bogus;z", "z & z", "trailing&"],
    );
}

#[test]
fn test_parse_html_replaces_invalid_numeric_references() {
    let html = r#"<!DOCTYPE html><html><body>
        <a href="a/">z&#0;z</a>
        <a href="b/">z&#xD800;z</a>
        <a href="c/">z&#4294967296;z</a>
        <a href="d/">z&#1114112;z</a>
        <a href="e/">z&#x80;z</a>
        <a href="f/">z&#x81;z</a>
        <a href="g/">z&#46 z</a>
        </body></html>"#;
    let parsed = parse_index_html(html, &Url::parse("https://pypi.org/simple/").unwrap()).unwrap();
    assert_eq!(
        parsed
            .projects
            .iter()
            .map(|project| project.name.as_str())
            .collect::<Vec<_>>(),
        vec![
            "z\u{FFFD}z",
            "z\u{FFFD}z",
            "z\u{FFFD}z",
            "z\u{FFFD}z",
            "z\u{20AC}z",
            "z\u{81}z",
            "z. z",
        ],
    );
}

#[test]
fn test_parse_index_html_decodes_html5_named_references_in_project_names() {
    let html = r#"<!DOCTYPE html><html><body>
        <a href="a/">foo&period;bar</a>
        <a href="b/">foo&lowbar;bar</a>
        <a href="c/">a&sol;b</a>
        <a href="d/">x&fjlig;y</a>
        </body></html>"#;
    let parsed = parse_index_html(html, &Url::parse("https://pypi.org/simple/").unwrap()).unwrap();
    assert_eq!(
        parsed
            .projects
            .iter()
            .map(|project| project.name.as_str())
            .collect::<Vec<_>>(),
        vec!["foo.bar", "foo_bar", "a/b", "xfjy"],
    );
}

#[test]
fn test_parse_index_html_decodes_semicolonless_named_references_in_text() {
    let html = r#"<!DOCTYPE html><html><body>
        <a href="a/">a&amp b</a>
        <a href="b/">c&copy d</a>
        </body></html>"#;
    let parsed = parse_index_html(html, &Url::parse("https://pypi.org/simple/").unwrap()).unwrap();
    assert_eq!(
        parsed
            .projects
            .iter()
            .map(|project| project.name.as_str())
            .collect::<Vec<_>>(),
        vec!["a& b", "c\u{A9} d"],
    );
}

#[test]
fn test_parse_detail_html_decodes_named_references_in_links_and_attributes() {
    let html = r#"<!DOCTYPE html><html><body>
        <a href="pkg&sol;dist&lowbar;1&period;0.tar.gz#sha256=2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae"
           data-requires-python="&gt;=3.8" data-yanked="see&period;notes">file</a>
        </body></html>"#;
    let parsed = parse_detail_html("flask", html, &base()).unwrap();
    let file = &parsed.files[0];
    assert_eq!(file.filename, "dist_1.0.tar.gz");
    assert_eq!(file.url, "https://pypi.org/simple/flask/pkg/dist_1.0.tar.gz");
    assert_eq!(
        file.hashes,
        BTreeMap::from([(
            "sha256".to_owned(),
            "2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae".to_owned()
        )]),
    );
    assert_eq!(file.requires_python.as_deref(), Some(">=3.8"));
    assert_eq!(file.yanked, Yanked::Reason("see.notes".to_owned()));
}

#[test]
fn test_parse_detail_html_keeps_ambiguous_ampersand_literal_in_attribute() {
    let html = r#"<!DOCTYPE html><html><body>
        <a href="pkg/file.whl?a=1&copy=2#sha256=ab">file</a>
        </body></html>"#;
    let parsed = parse_detail_html("flask", html, &base()).unwrap();
    let file = &parsed.files[0];
    assert_eq!(file.filename, "file.whl");
    assert_eq!(file.url, "https://pypi.org/simple/flask/pkg/file.whl?a=1&copy=2");
}

#[test]
fn test_parse_detail_html_applies_attribute_context_to_semicolonless_references() {
    let html = r#"<!DOCTYPE html><html><body>
        <a href="a/f.whl" data-yanked="x&amp y">a</a>
        <a href="b/f.whl" data-yanked="x&amp=y">b</a>
        <a href="c/f.whl" data-yanked="x&ampyz">c</a>
        </body></html>"#;
    let parsed = parse_detail_html("flask", html, &base()).unwrap();
    assert_eq!(
        parsed.files.iter().map(|file| &file.yanked).collect::<Vec<_>>(),
        vec![
            &Yanked::Reason("x& y".to_owned()),
            &Yanked::Reason("x&amp=y".to_owned()),
            &Yanked::Reason("x&ampyz".to_owned()),
        ],
    );
}

#[test]
fn test_fragment_folds_an_upper_case_sha256_to_the_served_content_address() {
    let html = r#"<a href="pkg-1.0.whl#sha256=9F86D081884C7D659A2FEAA0C55AD015A3BF4F1B2B0B822CD15D6C15B0F00A08">pkg-1.0.whl</a>"#;

    let file = &parse_detail_html("pkg", html, &base()).unwrap().files[0];

    assert_eq!(
        file.hashes,
        BTreeMap::from([(
            "sha256".to_owned(),
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08".to_owned()
        )])
    );
}

#[rstest::rstest]
#[case::truncated("9f86d081")]
#[case::overlong("9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a081")]
#[case::not_hex("9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00azz")]
fn test_fragment_drops_a_sha256_that_cannot_content_address(#[case] value: &str) {
    let html = format!(r#"<a href="pkg-1.0.whl#md5=deadbeef&sha256={value}">pkg-1.0.whl</a>"#);

    let file = &parse_detail_html("pkg", &html, &base()).unwrap().files[0];

    assert_eq!(file.hashes, BTreeMap::from([("md5".to_owned(), "deadbeef".to_owned())]));
}

#[test]
fn test_metadata_attr_folds_an_upper_case_sibling_digest() {
    let html = r#"<a href="pkg-1.0.whl" data-core-metadata="sha256=2C26B46B68FFC68FF99B453C1D30413413422D706483BFA0F98A5E886266E7AE">pkg-1.0.whl</a>"#;

    let file = &parse_detail_html("pkg", html, &base()).unwrap().files[0];

    assert_eq!(
        file.core_metadata,
        CoreMetadata::Hashes(BTreeMap::from([(
            "sha256".to_owned(),
            "2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae".to_owned()
        )]))
    );
}

#[test]
fn test_metadata_attr_drops_a_sibling_digest_that_cannot_content_address() {
    let html = r#"<a href="pkg-1.0.whl" data-core-metadata="sha256=deadbeef">pkg-1.0.whl</a>"#;

    let file = &parse_detail_html("pkg", html, &base()).unwrap().files[0];

    assert_eq!(file.core_metadata, CoreMetadata::Hashes(BTreeMap::new()));
}
