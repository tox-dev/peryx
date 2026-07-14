use peryx_core::UiBlock;
use rstest::rstest;

use crate::{file_matches_version, parse_metadata, ui_project_from_detail};

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
    assert!(project.files[0].yanked, "a reason string counts as yanked");
    assert!(project.files[0].has_metadata);
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
                Maintainer: Joe\n\
                Keywords: cache,index,proxy\n\
                Requires-Dist: requests>=2\n\
                Requires-Dist: click; extra == \"cli\"\n\
                Provides-Extra: cli\n\
                Classifier: Development Status :: 4 - Beta\n\
                Project-URL: Homepage, https://example.test\n\
                Home-Page: https://legacy.example.test\n\
                Description-Content-Type: text/markdown\n\
                \n\
                # peryxpkg\n\nThe long description.";
    let doc = parse_metadata(text);
    assert_eq!(doc.metadata_version.as_deref(), Some("2.4"));
    assert_eq!(doc.name, "peryxpkg");
    assert_eq!(doc.version, "1.2.0");
    assert_eq!(doc.summary.as_deref(), Some("A demo package"));
    assert_eq!(doc.requires_python.as_deref(), Some(">=3.8"));
    assert_eq!(doc.license, None);
    assert_eq!(doc.license_expression.as_deref(), Some("MIT"));
    assert_eq!(doc.license_files, ["LICENSE"]);
    assert_eq!(doc.author.as_deref(), Some("Jane"));
    assert_eq!(doc.maintainer.as_deref(), Some("Joe"));
    assert_eq!(doc.keywords, ["cache", "index", "proxy"]);
    assert_eq!(doc.requires_dist.len(), 2);
    assert_eq!(doc.provides_extra, ["cli"]);
    assert_eq!(doc.classifiers, ["Development Status :: 4 - Beta"]);
    assert_eq!(
        doc.project_urls,
        [("Homepage".to_owned(), "https://example.test".to_owned())]
    );
    assert_eq!(doc.home_page.as_deref(), Some("https://legacy.example.test"));
    assert_eq!(doc.description_content_type.as_deref(), Some("text/markdown"));
    assert!(doc.description.starts_with("# peryxpkg"));
}

#[test]
fn test_parse_metadata_preserves_keyword_spaces() {
    assert_eq!(
        parse_metadata("Name: x\nVersion: 1\nKeywords: machine learning, cache,,\n").keywords,
        ["machine learning", "cache"]
    );
}

#[test]
fn test_parse_metadata_description_header_and_folding() {
    let text = "Name: x\nVersion: 1\nDescription: first line\n continued here\n";
    let doc = parse_metadata(text);
    assert_eq!(doc.description, "first line continued here");
}

#[test]
fn test_parse_metadata_author_header_wins_over_email() {
    let text = "Name: x\nVersion: 1\nAuthor: Jane\nAuthor-email: jane@example.test\n";
    assert_eq!(parse_metadata(text).author.as_deref(), Some("Jane"));
}

#[test]
fn test_parse_metadata_uses_license_expression_for_display_when_license_is_absent() {
    let doc = parse_metadata("Name: x\nVersion: 1\nLicense-Expression: MIT\n");
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

#[test]
fn test_parse_metadata_skips_lines_without_colon() {
    let doc = parse_metadata("Name: x\ngarbage line\nVersion: 1\n");
    assert_eq!(doc.name, "x");
    assert_eq!(doc.version, "1");
}

#[test]
fn test_parse_metadata_ignores_unknown_headers() {
    let doc = parse_metadata("Name: x\nX-Internal: ignored\nVersion: 1\n");
    assert_eq!(doc.name, "x");
    assert_eq!(doc.version, "1");
}

#[test]
fn test_parse_metadata_splits_headers_from_body_on_a_crlf_blank_line() {
    // A CRLF document's header/body boundary is `\r\n\r\n`; matching only `\n\n` would read the body
    // as headers, so a `Version:` line in the description would overwrite the real version.
    let doc = parse_metadata("Name: x\r\nVersion: 1\r\n\r\nDescription body.\r\nVersion: 2 in prose.\r\n");
    assert_eq!(doc.name, "x");
    assert_eq!(doc.version, "1");
    assert_eq!(doc.description, "Description body.\r\nVersion: 2 in prose.");
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
    );
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
    let meta = crate::ui_meta(text);
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

    let bare = crate::ui_meta("Metadata-Version: 2.1\nName: p\nVersion: 1.0\n\n");
    assert!(
        !bare
            .blocks
            .iter()
            .any(|block| matches!(block, UiBlock::Groups { label, .. } if label == "Classifiers"))
    );
}
