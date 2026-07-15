use peryx_core::{UiBlock, UiRelease};
use rstest::rstest;

use crate::{MetadataError, file_matches_version, parse_metadata, ui_project_from_detail};

#[test]
fn test_ui_project_from_detail_maps_files() {
    let value = serde_json::json!({
        "name": "veloxdemo",
        "versions": ["1.0"],
        "files": [{
            "filename": "veloxdemo-1.0-py3-none-any.whl",
            "url": "/hosted/files/aa/veloxdemo-1.0-py3-none-any.whl",
            "hashes": {"sha256": "aa"},
            "size": 10,
            "upload-time": "2026-01-01T00:00:00Z",
            "yanked": "broken",
            "core-metadata": {"sha256": "bb"},
        }],
    });
    let project = ui_project_from_detail(&value);
    assert_eq!(project.name, "veloxdemo");
    assert_eq!(project.files[0].sha256, "aa");
    assert_eq!(project.files[0].upload_time.as_deref(), Some("2026-01-01T00:00:00Z"));
    assert!(project.files[0].has_metadata);
}

#[rstest]
#[case::reason(serde_json::json!("broken build"), true, Some("broken build"))]
#[case::without_reason(serde_json::json!(true), true, None)]
#[case::empty_reason(serde_json::json!(""), true, None)]
#[case::active(serde_json::json!(false), false, None)]
#[case::absent(serde_json::Value::Null, false, None)]
fn test_ui_project_from_detail_maps_yanked(
    #[case] yanked: serde_json::Value,
    #[case] expected: bool,
    #[case] reason: Option<&str>,
) {
    let value = serde_json::json!({
        "name": "veloxdemo",
        "files": [{"filename": "veloxdemo-1.0-py3-none-any.whl", "yanked": yanked}],
    });
    let project = ui_project_from_detail(&value);
    assert_eq!(
        (project.files[0].yanked, project.files[0].yanked_reason.as_deref()),
        (expected, reason)
    );
}

#[rstest]
#[case::every_file_yanked(&["1.0"], &[("1.0", r#""broken""#), ("1.0", "true")], true, &["broken"])]
#[case::one_active_file(&["1.0"], &[("1.0", r#""broken""#), ("1.0", "false")], false, &[])]
#[case::distinct_reasons(
    &["1.0"],
    &[("1.0", r#""broken""#), ("1.0", r#""broken""#), ("1.0", r#""unsafe""#)],
    true,
    &["broken", "unsafe"],
)]
#[case::reasonless_yank(&["1.0"], &[("1.0", "true"), ("1.0", r#""""#)], true, &[])]
#[case::pep440_equivalent_spelling(&["1.0.0"], &[("1.0", r#""broken""#)], true, &["broken"])]
#[case::release_without_files(&["1.0"], &[], false, &[])]
#[case::yank_of_another_release(&["1.0"], &[("2.0", "true")], false, &[])]
fn test_ui_project_from_detail_marks_a_release_its_publisher_yanked_whole(
    #[case] versions: &[&str],
    #[case] files: &[(&str, &str)],
    #[case] yanked: bool,
    #[case] reasons: &[&str],
) {
    assert_eq!(
        ui_project_from_detail(&detail_with_yanks(versions, files)).versions,
        [UiRelease {
            version: versions[0].to_owned(),
            yanked,
            yanked_reasons: reasons.iter().map(|reason| (*reason).to_owned()).collect(),
        }]
    );
}

#[test]
fn test_ui_project_from_detail_leaves_a_release_a_nameless_file_says_nothing_about() {
    let detail = serde_json::json!({
        "name": "veloxdemo",
        "versions": ["1.0"],
        "files": [
            {"filename": "notes.txt", "yanked": true},
            {"filename": "veloxdemo-1.0-py3-none-any.whl", "yanked": false},
        ],
    });

    assert_eq!(
        ui_project_from_detail(&detail).versions,
        [UiRelease {
            version: "1.0".to_owned(),
            yanked: false,
            yanked_reasons: vec![],
        }]
    );
}

/// A detail page declaring `versions`, with one wheel per `(version, yanked)` pair. The yank is the
/// JSON an index serves under PEP 592: `false`, `true`, or the reason.
fn detail_with_yanks(versions: &[&str], files: &[(&str, &str)]) -> serde_json::Value {
    let files: Vec<serde_json::Value> = files
        .iter()
        .enumerate()
        .map(|(index, (version, yanked))| {
            serde_json::json!({
                "filename": format!("veloxdemo-{version}-{index}-py3-none-any.whl"),
                "yanked": serde_json::from_str::<serde_json::Value>(yanked).expect("case spells yanked as JSON"),
            })
        })
        .collect();
    serde_json::json!({"name": "veloxdemo", "versions": versions, "files": files})
}

#[test]
fn test_parse_metadata_headers_and_body() {
    let text = "Metadata-Version: 2.4\n\
                Name: peryxpkg\n\
                Version: 1.2.0\n\
                Summary: A demo package\n\
                Requires-Python: >=3.8\n\
                License-Expression: MIT\n\
                License-File: LICENSE\n\
                Author: Jane\n\
                Author-email: jane@example.test\n\
                Maintainer: Joe\n\
                Maintainer-email: joe@example.test\n\
                Keywords: cache,index,proxy\n\
                Requires-Dist: requests>=2\n\
                Requires-Dist: click; extra == \"cli\"\n\
                Provides-Dist: virtual-package\n\
                Obsoletes-Dist: OldName (<3.0)\n\
                Provides-Extra: cli\n\
                Classifier: Development Status :: 4 - Beta\n\
                Dynamic: Requires-Dist\n\
                Project-URL: Homepage, https://example.test\n\
                Home-Page: https://legacy.example.test\n\
                Download-URL: https://legacy.example.test/downloads\n\
                Description-Content-Type: text/markdown\n\
                \n\
                # peryxpkg\n\nThe long description.";
    let doc = parse_metadata(text).unwrap();
    assert_eq!(doc.metadata_version.as_deref(), Some("2.4"));
    assert_eq!(doc.name, "peryxpkg");
    assert_eq!(doc.version, "1.2.0");
    assert_eq!(doc.summary.as_deref(), Some("A demo package"));
    assert_eq!(doc.requires_python.as_deref(), Some(">=3.8"));
    assert_eq!(doc.license, None);
    assert_eq!(doc.license_expression.as_deref(), Some("MIT"));
    assert_eq!(doc.license_files, ["LICENSE"]);
    assert_eq!(doc.author.as_deref(), Some("Jane"));
    assert_eq!(doc.author_email.as_deref(), Some("jane@example.test"));
    assert_eq!(doc.maintainer.as_deref(), Some("Joe"));
    assert_eq!(doc.maintainer_email.as_deref(), Some("joe@example.test"));
    assert_eq!(doc.keywords, ["cache", "index", "proxy"]);
    assert_eq!(doc.requires_dist.len(), 2);
    assert_eq!(doc.provides_dist, ["virtual-package"]);
    assert_eq!(doc.obsoletes_dist, ["OldName (<3.0)"]);
    assert_eq!(doc.provides_extra, ["cli"]);
    assert_eq!(doc.classifiers, ["Development Status :: 4 - Beta"]);
    assert_eq!(doc.dynamic, ["Requires-Dist"]);
    assert_eq!(
        doc.project_urls,
        [("Homepage".to_owned(), "https://example.test".to_owned())]
    );
    assert_eq!(doc.home_page.as_deref(), Some("https://legacy.example.test"));
    assert_eq!(
        doc.download_url.as_deref(),
        Some("https://legacy.example.test/downloads")
    );
    assert_eq!(doc.description_content_type.as_deref(), Some("text/markdown"));
    assert!(doc.description.starts_with("# peryxpkg"));
}

#[test]
fn test_parse_metadata_preserves_keyword_spaces() {
    assert_eq!(
        parse_metadata("Name: x\nVersion: 1\nKeywords: machine learning, cache,,\n")
            .unwrap()
            .keywords,
        ["machine learning", "cache"]
    );
}

#[test]
fn test_parse_metadata_description_header_and_folding() {
    let text = "Name: x\nVersion: 1\nDescription: first line\n continued here\n";
    let doc = parse_metadata(text).unwrap();
    assert_eq!(doc.description, "first line continued here");
}

// The description reaches the renderer verbatim; a re-added trim would fail every case here.
#[rstest]
#[case::indented_first_line(
    "Name: x\nVersion: 1\nDescription-Content-Type: text/markdown\n\n    print(\"hello\")\n",
    "    print(\"hello\")\n"
)]
#[case::surrounding_whitespace("Name: x\nVersion: 1\n\n  \nbody\n  \n", "  \nbody\n  \n")]
#[case::header_keeps_trailing("Name: x\nVersion: 1\nDescription: keep me  \n", "keep me  ")]
fn test_parse_metadata_preserves_description_payload(#[case] text: &str, #[case] description: &str) {
    assert_eq!(parse_metadata(text).unwrap().description, description);
}

#[test]
fn test_ui_meta_renders_an_indented_first_line_as_a_code_block() {
    let doc = parse_metadata("Name: x\nVersion: 1\nDescription-Content-Type: text/markdown\n\n    print(\"hello\")\n")
        .unwrap();
    let rendered = doc.to_ui_meta().description.expect("a described project renders one");
    assert!(rendered.html.contains("<pre><code>"), "{}", rendered.html);
    assert!(rendered.html.contains("print("), "{}", rendered.html);
}

#[rstest]
#[case::name_only("Author: Jane\n", Some("Jane"), None)]
#[case::email_only("Author-email: jane@example.test\n", None, Some("jane@example.test"))]
#[case::name_then_email(
    "Author: Jane\nAuthor-email: jane@example.test\n",
    Some("Jane"),
    Some("jane@example.test")
)]
#[case::email_then_name(
    "Author-email: jane@example.test\nAuthor: Jane\n",
    Some("Jane"),
    Some("jane@example.test")
)]
fn test_parse_metadata_keeps_author_name_and_email(
    #[case] headers: &str,
    #[case] name: Option<&str>,
    #[case] email: Option<&str>,
) {
    let doc = parse_metadata(&format!("Name: x\nVersion: 1\n{headers}")).unwrap();
    assert_eq!(doc.author.as_deref(), name);
    assert_eq!(doc.author_email.as_deref(), email);
}

#[rstest]
#[case::name_only("Maintainer: Joe\n", Some("Joe"), None)]
#[case::email_only("Maintainer-email: joe@example.test\n", None, Some("joe@example.test"))]
#[case::name_then_email(
    "Maintainer: Joe\nMaintainer-email: joe@example.test\n",
    Some("Joe"),
    Some("joe@example.test")
)]
#[case::email_then_name(
    "Maintainer-email: joe@example.test\nMaintainer: Joe\n",
    Some("Joe"),
    Some("joe@example.test")
)]
fn test_parse_metadata_keeps_maintainer_name_and_email(
    #[case] headers: &str,
    #[case] name: Option<&str>,
    #[case] email: Option<&str>,
) {
    let doc = parse_metadata(&format!("Name: x\nVersion: 1\n{headers}")).unwrap();
    assert_eq!(doc.maintainer.as_deref(), name);
    assert_eq!(doc.maintainer_email.as_deref(), email);
}

#[rstest]
#[case::name_first("Author: Jane\nAuthor-email: jane@x.test\nMaintainer: Joe\nMaintainer-email: joe@x.test\n")]
#[case::email_first("Author-email: jane@x.test\nAuthor: Jane\nMaintainer-email: joe@x.test\nMaintainer: Joe\n")]
fn test_ui_meta_exposes_contacts_in_a_fixed_order(#[case] headers: &str) {
    let doc = parse_metadata(&format!("Name: x\nVersion: 1\n{headers}")).unwrap();
    let meta = doc.to_ui_meta();
    let contacts: Vec<(&str, &str)> = meta
        .blocks
        .iter()
        .filter_map(|block| match block {
            UiBlock::KeyValue { label, value } => Some((label.as_str(), value.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(
        contacts,
        [
            ("Author", "Jane"),
            ("Author Email", "jane@x.test"),
            ("Maintainer", "Joe"),
            ("Maintainer Email", "joe@x.test"),
        ]
    );
}

#[test]
fn test_ui_meta_renders_the_description_and_chips() {
    let doc = parse_metadata(
        "Name: x\nVersion: 1\nKeywords: cache,proxy\nRequires-Dist: flask\n\
         Description-Content-Type: text/markdown\n\n**bold**",
    )
    .unwrap();
    let meta = doc.to_ui_meta();
    let rendered = meta.description.expect("a described project renders one");
    assert!(rendered.html.contains("<strong>bold</strong>"), "{}", rendered.html);
    assert!(rendered.notice.is_none());
    assert!(
        meta.blocks
            .iter()
            .any(|block| matches!(block, UiBlock::Chips { label, .. } if label == "Keywords"))
    );
    assert!(
        meta.blocks
            .iter()
            .any(|block| matches!(block, UiBlock::Chips { label, .. } if label == "Dependencies"))
    );
}

#[test]
fn test_ui_meta_leaves_no_rendered_description_without_one() {
    assert!(
        parse_metadata("Name: x\nVersion: 1\n")
            .unwrap()
            .to_ui_meta()
            .description
            .is_none()
    );
}

#[test]
fn test_parse_metadata_uses_license_expression_for_display_when_license_is_absent() {
    let doc = parse_metadata("Name: x\nVersion: 1\nLicense-Expression: MIT\n").unwrap();
    assert_eq!(doc.license, None);
    assert_eq!(doc.license_expression.as_deref(), Some("MIT"));
    assert_eq!(
        doc.to_ui_meta().blocks,
        [UiBlock::KeyValue {
            label: "License".to_owned(),
            value: "MIT".to_owned(),
        }]
    );
}

#[test]
fn test_ui_metadata_prefers_license_expression() {
    assert_eq!(
        parse_metadata("Name: x\nVersion: 1\nLicense: legacy\nLicense-Expression: MIT\n")
            .unwrap()
            .to_ui_meta()
            .blocks,
        [UiBlock::KeyValue {
            label: "License".to_owned(),
            value: "MIT".to_owned(),
        }]
    );
}

#[test]
fn test_file_matches_version_wheel_and_sdist() {
    assert!(file_matches_version("peryxpkg-1.0-py3-none-any.whl", "1.0"));
    assert!(file_matches_version("peryxpkg-1.0.tar.gz", "1.0"));
    assert!(file_matches_version("peryxpkg-1.0-py3-none-any.whl", "1.0.0")); // PEP 440 equal
    assert!(!file_matches_version("peryxpkg-1.0.1-py3-none-any.whl", "1.0"));
    assert!(!file_matches_version("peryxpkg-2.0-py3-none-any.whl", "1.0"));
    assert!(!file_matches_version("noversion.whl", "1.0"));
}

#[rstest]
#[case::missing_separator(
    "Metadata-Version: 2.4\nName: Flask\nmalformed header\nVersion: 1.0\n",
    MetadataError::MissingHeaderSeparator("malformed header".to_owned()),
    "header line \"malformed header\" is missing a colon"
)]
#[case::missing_name(
    "Name: Flask\n: 1.0\n",
    MetadataError::MissingHeaderName(": 1.0".to_owned()),
    "header line \": 1.0\" has no field name"
)]
#[case::leading_continuation(
    " Flask\nName: Flask\n",
    MetadataError::LeadingContinuation(" Flask".to_owned()),
    "document starts with the continuation line \" Flask\""
)]
fn test_parse_metadata_rejects_malformed_headers(
    #[case] text: &str,
    #[case] expected: MetadataError,
    #[case] message: &str,
) {
    let err = parse_metadata(text).unwrap_err();

    assert_eq!((err.to_string().as_str(), &err), (message, &expected));
}

#[rstest]
#[case::name("Metadata-Version: 2.1\nName: peryxpkg\nName: peryxpkg\nVersion: 1.0\n", "name")]
#[case::version("Metadata-Version: 2.1\nName: peryxpkg\nVersion: 1.0\nVersion: 2.0\n", "version")]
#[case::metadata_version(
    "Metadata-Version: 2.1\nMetadata-Version: 2.4\nName: p\nVersion: 1\n",
    "metadata-version"
)]
#[case::differing_case("Name: p\nVersion: 1\nSummary: one\nsummary: two\n", "summary")]
#[case::folded_repeat("Name: p\nVersion: 1\nAuthor: Jane\n Doe\nAuthor: June\n", "author")]
fn test_parse_metadata_rejects_repeated_single_use_field(#[case] text: &str, #[case] field: &str) {
    let err = parse_metadata(text).unwrap_err();

    assert_eq!(
        (err.to_string(), err),
        (
            format!("single-use field {field:?} appears more than once"),
            MetadataError::RepeatedField(field.to_owned())
        )
    );
}

#[rstest]
#[case::classifier("Classifier: Typing :: Typed\nClassifier: Framework :: Flask\n")]
#[case::requires_dist("Requires-Dist: flask\nRequires-Dist: click\n")]
#[case::project_url("Project-URL: Docs, https://docs.example\nProject-URL: Home, https://home.example\n")]
#[case::license_file("License-File: LICENSE\nLicense-File: NOTICE\n")]
#[case::provides_extra("Provides-Extra: dev\nProvides-Extra: test\n")]
#[case::distinct_single_use("Summary: one\nAuthor: Jane\nAuthor-email: jane@example.test\n")]
#[case::no_repeat("")]
fn test_parse_metadata_accepts_a_document_without_a_repeated_single_use_field(#[case] tail: &str) {
    let text = format!("Metadata-Version: 2.4\nName: peryxpkg\nVersion: 1.0\n{tail}");

    let doc = parse_metadata(&text).unwrap();

    assert_eq!((doc.name.as_str(), doc.version.as_str()), ("peryxpkg", "1.0"));
}

#[test]
fn test_parse_metadata_ignores_unknown_headers() {
    let doc = parse_metadata("Name: x\nX-Internal: ignored\nVersion: 1\n").unwrap();
    assert_eq!(doc.name, "x");
    assert_eq!(doc.version, "1");
}

// A document mixes line endings when its long description comes from a CRLF README, so the boundary
// holds at the first empty line either way and a `Version:` line in the prose never becomes a field.
#[rstest]
#[case::crlf_document(
    "Name: x\r\nVersion: 1\r\n\r\nDescription body.\r\nVersion: 2 in prose.\r\n",
    "Description body.\r\nVersion: 2 in prose.\r\n"
)]
#[case::crlf_body_below_lf_headers(
    "Name: x\nVersion: 1\n\nIntro.\r\n\r\nVersion: 2 in prose.\r\n",
    "Intro.\r\n\r\nVersion: 2 in prose.\r\n"
)]
fn test_parse_metadata_ends_the_header_block_at_the_first_empty_line(#[case] text: &str, #[case] description: &str) {
    let doc = parse_metadata(text).unwrap();

    assert_eq!((doc.version.as_str(), doc.description.as_str()), ("1", description));
}

#[test]
fn test_ui_meta_keeps_legacy_home_page_in_links() {
    let links = ui_links(
        "Metadata-Version: 2.1\nName: p\nVersion: 1.0\nProject-URL: Docs, https://docs.example\nHome-Page: https://home.example\n",
    );

    assert_eq!(
        links,
        [
            ("Documentation".to_owned(), "https://docs.example".to_owned()),
            ("Homepage".to_owned(), "https://home.example".to_owned())
        ]
    );
}

fn ui_links(text: &str) -> Vec<(String, String)> {
    crate::ui_meta(text)
        .unwrap()
        .blocks
        .into_iter()
        .find_map(|block| match block {
            UiBlock::Links { links, .. } => Some(links),
            _ => None,
        })
        .expect("a Links block")
}

#[rstest]
#[case::spelled_out("Bug Tracker", "Issue Tracker")]
#[case::snake_case("bug_tracker", "Issue Tracker")]
#[case::kebab_case("BUG-TRACKER", "Issue Tracker")]
#[case::alias("issues", "Issue Tracker")]
#[case::source_alias("GitHub", "Source Code")]
#[case::docs_alias("Docs", "Documentation")]
#[case::funding_alias("Donate", "Funding")]
#[case::unknown_label("Mastodon", "Mastodon")]
#[case::unknown_label_keeps_punctuation("Chat (Discord)", "Chat (Discord)")]
fn test_ui_meta_normalizes_well_known_project_url_labels(#[case] label: &str, #[case] displayed: &str) {
    let text = format!("Metadata-Version: 2.4\nName: p\nVersion: 1.0\nProject-URL: {label}, https://example.test\n");
    assert_eq!(
        ui_links(&text),
        [(displayed.to_owned(), "https://example.test".to_owned())]
    );
}

#[test]
fn test_parse_metadata_keeps_raw_project_url_labels() {
    // Normalization is presentation-only, so uploads and the Simple API keep the published label.
    let doc = parse_metadata(
        "Metadata-Version: 2.4\nName: p\nVersion: 1.0\nProject-URL: bug_tracker, https://bugs.example\n",
    )
    .unwrap();
    assert_eq!(
        doc.project_urls,
        [("bug_tracker".to_owned(), "https://bugs.example".to_owned())]
    );
}

#[test]
fn test_ui_meta_groups_classifiers_and_omits_the_block_when_none() {
    // Two classifiers share the "Programming Language" category, so the second appends to the first
    // group; a third opens its own. Classifier-less metadata emits no Classifiers block at all.
    let text = "Metadata-Version: 2.1\nName: p\nVersion: 1.0\n\
                Classifier: Programming Language :: Python :: 3.8\n\
                Classifier: Programming Language :: Python :: 3.9\n\
                Classifier: License :: OSI Approved\n\n";
    let meta = crate::ui_meta(text).unwrap();
    let groups = meta
        .blocks
        .iter()
        .find_map(|block| match block {
            UiBlock::Groups { label, groups } if label == "Classifiers" => Some(groups),
            _ => None,
        })
        .expect("a Classifiers block");
    assert_eq!(
        groups,
        &vec![
            (
                "Programming Language".to_owned(),
                vec!["Python :: 3.8".to_owned(), "Python :: 3.9".to_owned()],
            ),
            ("License".to_owned(), vec!["OSI Approved".to_owned()]),
        ]
    );

    let bare = crate::ui_meta("Metadata-Version: 2.1\nName: p\nVersion: 1.0\n\n").unwrap();
    assert!(
        !bare
            .blocks
            .iter()
            .any(|block| matches!(block, UiBlock::Groups { label, .. } if label == "Classifiers"))
    );
}
