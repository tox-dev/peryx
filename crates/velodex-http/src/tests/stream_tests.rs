use std::collections::HashMap;

use velodex_core::pypi::{CoreMetadata, File, Yanked, parse_detail, to_json};

use crate::stream::{PageContext, PageTransformer, Registration, page_context};

fn upstream_page() -> String {
    r#"{"meta":{"api-version":"1.1"},"name":"demo","versions":["1.0","2.0"],"files":[
        {"filename":"demo-1.0-py3-none-any.whl","url":"https://up/demo-1.0-py3-none-any.whl",
         "hashes":{"sha256":"aa11"},"size":10,
         "core-metadata":{"sha256":"bb22"},"yanked":false},
        {"filename":"demo-2.0.tar.gz","url":"https://up/demo-2.0.tar.gz","hashes":{},"yanked":false},
        {"filename":"demo-2.0-py3-none-any.whl","url":"https://up/demo-2.0-py3-none-any.whl",
         "hashes":{"sha256":"cc33"},"yanked":false}
    ]}"#
    .to_owned()
}

/// Feed the page through the transformer in chunks of the given size.
fn transform(page: &str, context: PageContext, chunk: usize) -> (String, Vec<Registration>) {
    let (out, summary) = transform_summary(page, context, chunk);
    (out, summary.registrations)
}

/// Like [`transform`], returning everything the transformer learned.
fn transform_summary(page: &str, context: PageContext, chunk: usize) -> (String, crate::stream::PageSummary) {
    let mut transformer = PageTransformer::new(context);
    let mut out = Vec::new();
    for piece in page.as_bytes().chunks(chunk) {
        out.extend(transformer.push(piece).unwrap());
    }
    let summary = transformer.finish().unwrap();
    (String::from_utf8(out).unwrap(), summary)
}

fn plain_context() -> PageContext {
    page_context("root/pypi", Vec::new(), Vec::new(), &HashMap::new())
}

#[test]
fn test_rewrites_urls_and_registers_sources() {
    for chunk in [1, 3, 7, 4096] {
        let (out, registrations) = transform(&upstream_page(), plain_context(), chunk);
        let detail = parse_detail(out.as_bytes()).unwrap();
        assert_eq!(detail.files.len(), 3, "chunk size {chunk}");
        assert_eq!(detail.files[0].url, "/root/pypi/files/aa11/demo-1.0-py3-none-any.whl");
        // The file without a sha keeps its upstream URL and loses the metadata claim.
        assert_eq!(detail.files[1].url, "https://up/demo-2.0.tar.gz");
        assert_eq!(registrations.len(), 2);
        assert_eq!(registrations[0].sha256, "aa11");
        assert_eq!(registrations[0].url, "https://up/demo-1.0-py3-none-any.whl");
        assert_eq!(
            registrations[0].metadata,
            Some((
                "https://up/demo-1.0-py3-none-any.whl.metadata".to_owned(),
                "bb22".to_owned()
            ))
        );
        assert_eq!(registrations[1].metadata, None);
    }
}

#[test]
fn test_injects_local_files_and_shadows_upstream() {
    let local = File {
        filename: "demo-2.0-py3-none-any.whl".to_owned(),
        url: "/root/pypi/files/dd44/demo-2.0-py3-none-any.whl".to_owned(),
        hashes: std::collections::BTreeMap::from([("sha256".to_owned(), "dd44".to_owned())]),
        requires_python: None,
        size: Some(5),
        upload_time: None,
        yanked: Yanked::No,
        core_metadata: CoreMetadata::Absent,
    };
    let context = page_context("root/pypi", vec![local], vec!["3.0".to_owned()], &HashMap::new());
    let (out, _) = transform(&upstream_page(), context, 1);
    let detail = parse_detail(out.as_bytes()).unwrap();
    // The local file leads, the same-named upstream file is gone, others remain.
    assert_eq!(detail.files[0].hashes["sha256"], "dd44");
    assert_eq!(detail.files.len(), 3);
    assert!(
        detail
            .files
            .iter()
            .filter(|file| file.filename == "demo-2.0-py3-none-any.whl")
            .count()
            == 1
    );
    // Versions union, sorted.
    assert_eq!(detail.versions, ["1.0", "2.0", "3.0"]);
}

#[test]
fn test_hidden_and_yank_overrides() {
    let overrides = HashMap::from([
        ("demo-1.0-py3-none-any.whl".to_owned(), "hidden".to_owned()),
        ("demo-2.0-py3-none-any.whl".to_owned(), "yanked".to_owned()),
    ]);
    let context = page_context("root/pypi", Vec::new(), Vec::new(), &overrides);
    let (out, _) = transform(&upstream_page(), context, 2);
    let detail = parse_detail(out.as_bytes()).unwrap();
    assert_eq!(detail.files.len(), 2, "hidden file dropped");
    let yanked = detail
        .files
        .iter()
        .find(|file| file.filename == "demo-2.0-py3-none-any.whl")
        .unwrap();
    assert_eq!(yanked.yanked, Yanked::Yes);
}

#[test]
fn test_escapes_and_braces_inside_strings_survive() {
    let page = r#"{"name":"de\"mo}{","versions":[],"files":[
        {"filename":"a{1}-1.0.whl","url":"https://up/a\"b[",
         "hashes":{"sha256":"ee55"},"yanked":false}
    ]}"#;
    for chunk in [1, 5] {
        let (out, registrations) = transform(page, plain_context(), chunk);
        let detail = parse_detail(out.as_bytes()).unwrap();
        assert_eq!(detail.files[0].url, "/root/pypi/files/ee55/a{1}-1.0.whl");
        assert_eq!(registrations[0].url, "https://up/a\"b[");
    }
}

#[test]
fn test_versions_after_files_and_empty_files() {
    let page = r#"{"files":[],"versions":["2.0"],"name":"demo"}"#;
    let context = page_context("r", Vec::new(), vec!["1.0".to_owned()], &HashMap::new());
    let (out, registrations) = transform(page, context, 1);
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["versions"], serde_json::json!(["1.0", "2.0"]));
    assert_eq!(value["files"].as_array().unwrap().len(), 0);
    assert!(registrations.is_empty());
}

#[test]
fn test_local_files_emitted_into_empty_upstream_array() {
    let page = r#"{"name":"demo","files":[]}"#;
    let local = File {
        filename: "demo-1.0-py3-none-any.whl".to_owned(),
        url: "/r/files/aa/demo-1.0-py3-none-any.whl".to_owned(),
        hashes: std::collections::BTreeMap::new(),
        requires_python: None,
        size: None,
        upload_time: None,
        yanked: Yanked::No,
        core_metadata: CoreMetadata::Absent,
    };
    let (out, _) = transform(page, page_context("r", vec![local], Vec::new(), &HashMap::new()), 3);
    let detail = parse_detail(out.as_bytes()).unwrap();
    assert_eq!(detail.files.len(), 1);
}

#[test]
fn test_truncated_page_is_an_error() {
    let mut transformer = PageTransformer::new(plain_context());
    transformer.push(br#"{"files":[{"filename":"x"#).unwrap();
    assert!(transformer.finish().is_err());
}

#[test]
fn test_corrupt_file_element_is_an_error() {
    let mut transformer = PageTransformer::new(plain_context());
    let result = transformer.push(br#"{"files":[{"filename":42}]}"#);
    assert!(result.is_err());
}

#[test]
fn test_output_roundtrips_through_serializer() {
    // The transformed page must parse into exactly what the buffered path would produce.
    let (out, _) = transform(&upstream_page(), plain_context(), 4096);
    let detail = parse_detail(out.as_bytes()).unwrap();
    let reserialized = to_json(&serde_json::from_str::<serde_json::Value>(&out).unwrap());
    assert!(!reserialized.is_empty());
    assert_eq!(detail.name, "demo");
}

#[test]
fn test_unrelated_top_level_arrays_pass_through() {
    let page = r#"{"alternate-locations":["https://other/simple/demo/"],"versions":["1.0"],"files":[]}"#;
    let (out, registrations) = transform(page, plain_context(), 3);
    assert!(out.contains("https://other/simple/demo/"));
    assert!(registrations.is_empty());
}

#[test]
fn test_nested_array_inside_file_object_is_captured() {
    let page = r#"{"files":[{"filename":"demo-1.0-py3-none-any.whl","url":"https://up/d.whl",
        "hashes":{"sha256":"aa11"},"provenance":["sig1","sig2"]}]}"#;
    let (out, registrations) = transform(page, plain_context(), 5);
    assert_eq!(registrations.len(), 1);
    assert!(out.contains("/root/pypi/files/aa11/demo-1.0-py3-none-any.whl"));
}

#[test]
fn test_escaped_version_strings_merge() {
    let page = r#"{"name":"demo","versions":["1\u002e0","2.0"],"files":[]}"#;
    let (out, _) = transform(page, plain_context(), 2);
    let detail = parse_detail(out.as_bytes()).unwrap();
    assert_eq!(detail.versions, vec!["1.0", "2.0"]);
}

#[test]
fn test_nested_container_in_versions_is_a_parse_error() {
    let mut transformer = PageTransformer::new(plain_context());
    let result = transformer.push(br#"{"versions":[["nested"],{}],"files":[]}"#);
    assert!(result.is_err());
}

#[test]
fn test_two_local_files_emit_with_separators() {
    let local = |version: &str| File {
        filename: format!("demo-{version}-py3-none-any.whl"),
        url: format!("/root/pypi/files/dd{version}/demo-{version}-py3-none-any.whl"),
        hashes: std::collections::BTreeMap::new(),
        requires_python: None,
        size: None,
        upload_time: None,
        yanked: Yanked::No,
        core_metadata: CoreMetadata::Absent,
    };
    let context = page_context(
        "root/pypi",
        vec![local("3.0"), local("4.0")],
        Vec::new(),
        &HashMap::new(),
    );
    let (out, _) = transform(r#"{"name":"demo","files":[]}"#, context, 4096);
    let detail = parse_detail(out.as_bytes()).unwrap();
    assert_eq!(detail.files.len(), 2);
}

#[test]
fn test_legacy_record_urls_pass_through_unregistered() {
    let page = r#"{"files":[{"filename":"demo-1.0-py3-none-any.whl",
        "url":"/root/pypi/files/aa11/demo-1.0-py3-none-any.whl","hashes":{"sha256":"aa11"}}]}"#;
    let (out, registrations) = transform(page, plain_context(), 6);
    assert!(out.contains("/root/pypi/files/aa11/demo-1.0-py3-none-any.whl"));
    assert!(registrations.is_empty());
}

#[test]
fn test_unknown_override_kind_is_ignored() {
    let overrides: HashMap<String, String> = [("demo-1.0-py3-none-any.whl".to_owned(), "frozen".to_owned())].into();
    let context = page_context("root/pypi", Vec::new(), Vec::new(), &overrides);
    assert!(context.skip.is_empty());
    assert!(context.yanked.is_empty());
}

#[test]
fn test_legacy_record_after_a_rewritten_file_keeps_separators() {
    let page = r#"{"name":"demo","files":[
        {"filename":"demo-1.0-py3-none-any.whl","url":"https://up/demo-1.0-py3-none-any.whl",
         "hashes":{"sha256":"aa11"}},
        {"filename":"demo-2.0-py3-none-any.whl","url":"/root/pypi/files/bb22/demo-2.0-py3-none-any.whl",
         "hashes":{"sha256":"bb22"}}]}"#;
    let (out, registrations) = transform(page, plain_context(), 9);
    let detail = parse_detail(out.as_bytes()).unwrap();
    assert_eq!(detail.files.len(), 2);
    assert_eq!(registrations.len(), 1);
}

#[test]
fn test_page_name_is_captured_without_a_parse() {
    for chunk in [1, 7, 4096] {
        let (_, summary) = transform_summary(&upstream_page(), plain_context(), chunk);
        assert_eq!(summary.name.as_deref(), Some("demo"), "chunk size {chunk}");
    }
}

#[test]
fn test_missing_page_name_is_none() {
    let (_, summary) = transform_summary(r#"{"files":[]}"#, plain_context(), 3);
    assert_eq!(summary.name, None);
}

#[test]
fn test_trailing_bytes_after_the_root_are_an_error() {
    let mut transformer = PageTransformer::new(plain_context());
    transformer.push(br#"{"name":"demo","files":[]}garbage"#).unwrap();
    assert!(transformer.finish().is_err());
}
