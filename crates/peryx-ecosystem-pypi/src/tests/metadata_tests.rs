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
    let text = "Metadata-Version: 2.1\n\
                Name: peryxpkg\n\
                Version: 1.2.0\n\
                Summary: A demo package\n\
                Requires-Python: >=3.8\n\
                License: MIT\n\
                License-Expression: MIT\n\
                License-File: LICENSE\n\
                Author: Jane\n\
                Maintainer: Joe\n\
                Keywords: cache,index proxy\n\
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
    assert_eq!(doc.metadata_version.as_deref(), Some("2.1"));
    assert_eq!(doc.name, "peryxpkg");
    assert_eq!(doc.version, "1.2.0");
    assert_eq!(doc.summary.as_deref(), Some("A demo package"));
    assert_eq!(doc.requires_python.as_deref(), Some(">=3.8"));
    assert_eq!(doc.license.as_deref(), Some("MIT"));
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
        [
            ("Homepage".to_owned(), "https://example.test".to_owned()),
            ("Homepage".to_owned(), "https://legacy.example.test".to_owned())
        ]
    );
    assert_eq!(doc.description_content_type.as_deref(), Some("text/markdown"));
    assert!(doc.description.starts_with("# peryxpkg"));
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
    assert_eq!(doc.license.as_deref(), Some("MIT"));
    assert_eq!(doc.license_expression.as_deref(), Some("MIT"));
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
fn test_ui_meta_groups_classifiers_and_omits_the_block_when_none() {
    use peryx_core::UiBlock;

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
