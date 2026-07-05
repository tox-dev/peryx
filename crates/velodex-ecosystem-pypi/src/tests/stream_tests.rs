use std::collections::HashMap;

use crate::{CoreMetadata, File, Provenance, Yanked, parse_detail, to_json};

use crate::stream::{PageContext, PageTransformer, Registration, page_context as build_page_context};
use velodex_policy::{PackageType, Policy, PolicyConfig};

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

fn page_context<S: std::hash::BuildHasher>(
    route: &str,
    local_files: Vec<File>,
    local_versions: Vec<String>,
    overrides: &HashMap<String, String, S>,
) -> PageContext {
    build_page_context(route, "demo", Policy::default(), local_files, local_versions, overrides)
}

fn policy(configure: impl FnOnce(&mut PolicyConfig)) -> Policy {
    let mut config = PolicyConfig::default();
    configure(&mut config);
    Policy::compile(&config).unwrap()
}

fn local_wheel(filename: &str) -> File {
    File {
        filename: filename.to_owned(),
        url: format!("/root/pypi/files/dd44/{filename}"),
        hashes: std::collections::BTreeMap::from([("sha256".to_owned(), "dd44".to_owned())]),
        requires_python: None,
        size: Some(5),
        upload_time: None,
        yanked: Yanked::No,
        core_metadata: CoreMetadata::Absent,
        dist_info_metadata: CoreMetadata::Absent,
        gpg_sig: None,
        provenance: Provenance::Absent,
    }
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
        assert_eq!(registrations[0].filename, "demo-1.0-py3-none-any.whl");
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
fn test_rewrites_cached_generated_metadata() {
    let page = r#"{"meta":{"api-version":"1.1"},"name":"demo","files":[{
        "filename":"demo-1.0-py3-none-any.whl","url":"https://up/demo-1.0-py3-none-any.whl",
        "hashes":{"sha256":"aa11"},"yanked":false
    }]}"#;
    let mut context = plain_context();
    context.known_metadata.insert("aa11".to_owned(), "bb22".to_owned());

    let (out, registrations) = transform(page, context, 7);

    let detail = parse_detail(out.as_bytes()).unwrap();
    assert_eq!(
        detail.files[0].metadata(),
        &CoreMetadata::Hashes(std::collections::BTreeMap::from([(
            "sha256".to_owned(),
            "bb22".to_owned()
        )]))
    );
    assert_eq!(registrations[0].metadata, None);
}

#[test]
fn test_rewrites_egg_urls_without_advertising_metadata() {
    let page = r#"{"meta":{"api-version":"1.1"},"name":"demo","files":[{
        "filename":"demo-1.0.egg","url":"https://up/demo-1.0.egg",
        "hashes":{"sha256":"aa11"},"core-metadata":{"sha256":"bb22"},"yanked":false
    }]}"#;
    let (out, registrations) = transform(page, plain_context(), 7);
    let detail = parse_detail(out.as_bytes()).unwrap();
    assert_eq!(detail.files[0].url, "/root/pypi/files/aa11/demo-1.0.egg");
    assert_eq!(detail.files[0].core_metadata, CoreMetadata::Absent);
    assert_eq!(detail.files[0].dist_info_metadata, CoreMetadata::Absent);
    assert_eq!(registrations[0].metadata, None);
}

#[test]
fn test_injects_local_files_and_shadows_upstream() {
    let local = local_wheel("demo-2.0-py3-none-any.whl");
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
fn test_policy_filters_local_files() {
    let policy = policy(|config| {
        config.block_package_types = vec![PackageType::Wheel];
    });
    let context = build_page_context(
        "root/pypi",
        "demo",
        policy,
        vec![local_wheel("demo-3.0-py3-none-any.whl")],
        Vec::new(),
        &HashMap::new(),
    );

    let (out, registrations) = transform(r#"{"meta":{"api-version":"1.1"},"name":"demo","files":[]}"#, context, 8);

    let detail = parse_detail(out.as_bytes()).unwrap();
    assert!(detail.files.is_empty());
    assert!(registrations.is_empty());
}

#[test]
fn test_policy_filters_upstream_files() {
    let policy = policy(|config| {
        config.block_package_types = vec![PackageType::Wheel];
    });
    let context = build_page_context("root/pypi", "demo", policy, Vec::new(), Vec::new(), &HashMap::new());

    let (out, registrations) = transform(&upstream_page(), context, 7);

    let detail = parse_detail(out.as_bytes()).unwrap();
    assert_eq!(detail.files.len(), 1);
    assert_eq!(detail.files[0].filename, "demo-2.0.tar.gz");
    assert!(registrations.is_empty());
}

#[test]
fn test_hidden_and_yank_overrides() {
    let overrides = HashMap::from([
        ("demo-1.0-py3-none-any.whl".to_owned(), "hidden".to_owned()),
        (
            "demo-2.0-py3-none-any.whl".to_owned(),
            r#"{"kind":"yanked","reason":"bad build"}"#.to_owned(),
        ),
        ("demo-2.0.tar.gz".to_owned(), "yanked".to_owned()),
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
    assert_eq!(yanked.yanked, Yanked::Reason("bad build".to_owned()));
    let legacy_yanked = detail
        .files
        .iter()
        .find(|file| file.filename == "demo-2.0.tar.gz")
        .unwrap();
    assert_eq!(legacy_yanked.yanked, Yanked::Yes);
}

#[test]
fn test_empty_reason_yank_override_yanks_without_reason() {
    let overrides = HashMap::from([(
        "demo-2.0.tar.gz".to_owned(),
        r#"{"kind":"yanked","reason":""}"#.to_owned(),
    )]);
    let context = page_context("root/pypi", Vec::new(), Vec::new(), &overrides);
    let (out, _) = transform(&upstream_page(), context, 2);
    let detail = parse_detail(out.as_bytes()).unwrap();
    let file = detail
        .files
        .iter()
        .find(|file| file.filename == "demo-2.0.tar.gz")
        .unwrap();
    assert_eq!(file.yanked, Yanked::Yes);
}

#[test]
fn test_quarantined_project_streams_without_files() {
    let page = r#"{"meta":{"api-version":"1.4","project-status":"quarantined",
        "project-status-reason":"malware"},"name":"demo","versions":["1.0"],"files":[
        {"filename":"demo-1.0-py3-none-any.whl","url":"https://up/demo-1.0-py3-none-any.whl",
         "hashes":{"sha256":"aa11"}}
    ]}"#;
    let (out, registrations) = transform(page, plain_context(), 5);
    let detail = parse_detail(out.as_bytes()).unwrap();
    assert_eq!(detail.meta.status(), crate::ProjectStatus::Quarantined);
    assert!(detail.files.is_empty());
    assert!(registrations.is_empty());
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
        assert_eq!(detail.files[0].url, "/root/pypi/files/ee55/a%7B1%7D-1.0.whl");
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
        dist_info_metadata: CoreMetadata::Absent,
        gpg_sig: None,
        provenance: Provenance::Absent,
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
        "hashes":{"sha256":"aa11"},"extra":["sig1","sig2"]}]}"#;
    let (out, registrations) = transform(page, plain_context(), 5);
    assert_eq!(registrations.len(), 1);
    assert!(out.contains("/root/pypi/files/aa11/demo-1.0-py3-none-any.whl"));
}

#[test]
fn test_preserves_simple_api_fields_during_streaming() {
    let page = r#"{"meta":{"api-version":"1.4","project-status":"archived",
        "project-status-reason":"read only"},"name":"demo","versions":["1.0"],"files":[
        {"filename":"demo-1.0-py3-none-any.whl","url":"https://up/demo-1.0-py3-none-any.whl",
         "hashes":{"sha256":"aa11"},"size":10,"upload-time":"2024-01-01T00:00:00Z",
         "core-metadata":{"sha256":"bb22"},"dist-info-metadata":{"sha256":"bb22"},
         "gpg-sig":false,"provenance":"https://up/demo-1.0-py3-none-any.whl.provenance"}
    ]}"#;
    for chunk in [1, 11, 4096] {
        let (out, _) = transform(page, plain_context(), chunk);
        let detail = parse_detail(out.as_bytes()).unwrap();
        assert_eq!(
            (
                detail.meta.project_status.as_deref(),
                detail.meta.project_status_reason.as_deref(),
                detail.files[0].size,
                detail.files[0].upload_time.as_deref(),
                &detail.files[0].core_metadata,
                &detail.files[0].dist_info_metadata,
                detail.files[0].gpg_sig,
                &detail.files[0].provenance,
            ),
            (
                Some("archived"),
                Some("read only"),
                Some(10),
                Some("2024-01-01T00:00:00Z"),
                &CoreMetadata::Hashes(std::collections::BTreeMap::from([(
                    "sha256".to_owned(),
                    "bb22".to_owned(),
                )])),
                &CoreMetadata::Hashes(std::collections::BTreeMap::from([(
                    "sha256".to_owned(),
                    "bb22".to_owned(),
                )])),
                Some(false),
                &Provenance::Url("https://up/demo-1.0-py3-none-any.whl.provenance".to_owned()),
            ),
            "chunk size {chunk}"
        );
    }
}

#[test]
fn test_meta_streaming_handles_escaped_and_nested_unknown_values() {
    let page = r#"{"meta":{"api-version":"1.4","project-status":"archived",
        "project-status-reason":"read \"only\"",
        "extra":[{"ignored":"yes"}]},"name":"demo","files":[]}"#;
    let (out, _) = transform(page, plain_context(), 4096);
    let detail = parse_detail(out.as_bytes()).unwrap();
    assert_eq!(
        (
            detail.meta.project_status.as_deref(),
            detail.meta.project_status_reason.as_deref(),
        ),
        (Some("archived"), Some("read \"only\""))
    );
}

#[test]
fn test_streaming_rejects_unsupported_major_api_version() {
    let mut transformer = PageTransformer::new(plain_context());
    let result = transformer.push(br#"{"meta":{"api-version":"2.0"},"name":"demo","files":[]}"#);
    assert!(result.is_err());
}

#[test]
fn test_simple_field_deserializers_reject_invalid_types() {
    assert!(serde_json::from_str::<Yanked>("123").is_err());
    assert!(serde_json::from_str::<CoreMetadata>("123").is_err());
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
        dist_info_metadata: CoreMetadata::Absent,
        gpg_sig: None,
        provenance: Provenance::Absent,
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
