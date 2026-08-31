use std::collections::BTreeMap;

use peryx_policy::{Policy, PolicyConfig};
use rstest::rstest;

use crate::policy::RemoteMetadataMode;

use super::page_context;
use crate::policy::{PackageType, PypiPolicyConfig, compile_capabilities};
use crate::store::FileOverride;
use crate::stream::{
    PageContext, PageSummary, PageTransformer, Registration, TransformError, page_context as build_page_context,
};
use crate::{CoreMetadata, File, Provenance, Yanked, parse_detail, to_json};

fn upstream_page() -> String {
    r#"{"meta":{"api-version":"1.1"},"name":"demo","versions":["1.0","2.0"],"files":[
        {"filename":"demo-1.0-py3-none-any.whl","url":"https://up/demo-1.0-py3-none-any.whl",
         "hashes":{"sha256":"aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11"},"size":10,
         "core-metadata":{"sha256":"bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22"},"yanked":false},
        {"filename":"demo-2.0.tar.gz","url":"https://up/demo-2.0.tar.gz","hashes":{},"size":20,"yanked":false},
        {"filename":"demo-2.0-py3-none-any.whl","url":"https://up/demo-2.0-py3-none-any.whl",
         "hashes":{"sha256":"cc33cc33cc33cc33cc33cc33cc33cc33cc33cc33cc33cc33cc33cc33cc33cc33"},"size":30,"yanked":false}
    ]}"#
    .to_owned()
}

fn transform(page: &str, context: PageContext, chunk: usize) -> (String, Vec<Registration>) {
    let (out, summary) = transform_summary(page, context, chunk);
    (out, summary.registrations)
}

fn transform_summary(page: &str, context: PageContext, chunk: usize) -> (String, crate::stream::PageSummary) {
    let mut transformer = PageTransformer::new(context);
    let mut out = Vec::new();
    for piece in page.as_bytes().chunks(chunk) {
        transformer.push_into(piece, &mut out).unwrap();
    }
    let summary = transformer.finish().unwrap();
    (String::from_utf8(out).unwrap(), summary)
}

fn plain_context() -> PageContext {
    page_context("root/pypi", Vec::new(), Vec::new(), &BTreeMap::new())
}

fn policy(configure: impl FnOnce(&mut PypiPolicyConfig)) -> Policy {
    let mut config = PypiPolicyConfig::default();
    configure(&mut config);
    Policy::compile(&PolicyConfig::default(), crate::normalize_name)
        .with_capabilities(compile_capabilities(&config).unwrap())
}

fn local_wheel(filename: &str) -> File {
    File {
        filename: filename.to_owned(),
        url: format!("/root/pypi/files/dd44dd44dd44dd44dd44dd44dd44dd44dd44dd44dd44dd44dd44dd44dd44dd44/{filename}"),
        hashes: std::collections::BTreeMap::from([(
            "sha256".to_owned(),
            "dd44dd44dd44dd44dd44dd44dd44dd44dd44dd44dd44dd44dd44dd44dd44dd44".to_owned(),
        )]),
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
        assert_eq!(
            detail.files[0].url,
            "/root/pypi/files/aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11/demo-1.0-py3-none-any.whl"
        );
        // The file without a sha keeps its upstream URL and loses the metadata claim.
        assert_eq!(detail.files[1].url, "https://up/demo-2.0.tar.gz");
        assert_eq!(registrations.len(), 2);
        assert_eq!(registrations[0].filename, "demo-1.0-py3-none-any.whl");
        assert_eq!(
            registrations[0].sha256,
            "aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11"
        );
        assert_eq!(registrations[0].url, "https://up/demo-1.0-py3-none-any.whl");
        assert_eq!(
            registrations[0].metadata,
            Some((
                "https://up/demo-1.0-py3-none-any.whl.metadata".to_owned(),
                "bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22".to_owned()
            ))
        );
        assert_eq!(registrations[1].metadata, None);
    }
}

#[test]
fn test_rewrites_cached_generated_metadata() {
    let page = r#"{"meta":{"api-version":"1.1"},"versions":[],"name":"demo","files":[{
        "filename":"demo-1.0-py3-none-any.whl","size":11,"url":"https://up/demo-1.0-py3-none-any.whl",
        "hashes":{"sha256":"aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11"},"yanked":false
    }]}"#;
    let mut context = plain_context();
    context.known_metadata.insert(
        "aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11".to_owned(),
        "bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22".to_owned(),
    );

    let (out, registrations) = transform(page, context, 7);

    let detail = parse_detail(out.as_bytes()).unwrap();
    assert_eq!(
        detail.files[0].metadata(),
        &CoreMetadata::Hashes(std::collections::BTreeMap::from([(
            "sha256".to_owned(),
            "bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22".to_owned()
        )]))
    );
    assert_eq!(registrations[0].metadata, None);
}

#[test]
fn test_rewrites_egg_urls_without_advertising_metadata() {
    let page = r#"{"meta":{"api-version":"1.1"},"versions":[],"name":"demo","files":[{
        "filename":"demo-1.0.egg","size":11,"url":"https://up/demo-1.0.egg",
        "hashes":{"sha256":"aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11"},"core-metadata":{"sha256":"bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22"},"yanked":false
    }]}"#;
    let (out, registrations) = transform(page, plain_context(), 7);
    let detail = parse_detail(out.as_bytes()).unwrap();
    assert_eq!(
        detail.files[0].url,
        "/root/pypi/files/aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11/demo-1.0.egg"
    );
    assert_eq!(detail.files[0].core_metadata, CoreMetadata::Absent);
    assert_eq!(detail.files[0].dist_info_metadata, CoreMetadata::Absent);
    assert_eq!(registrations[0].metadata, None);
}

#[test]
fn test_injects_local_files_and_shadows_upstream() {
    let local = local_wheel("demo-2.0-py3-none-any.whl");
    let context = page_context("root/pypi", vec![local], vec!["3.0".to_owned()], &BTreeMap::new());
    let (out, _) = transform(&upstream_page(), context, 1);
    let detail = parse_detail(out.as_bytes()).unwrap();

    assert_eq!(
        detail.files[0].hashes["sha256"],
        "dd44dd44dd44dd44dd44dd44dd44dd44dd44dd44dd44dd44dd44dd44dd44dd44"
    );
    assert_eq!(detail.files.len(), 3);
    assert_eq!(
        detail
            .files
            .iter()
            .filter(|file| file.filename == "demo-2.0-py3-none-any.whl")
            .count(),
        1
    );

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
        &BTreeMap::new(),
    );

    let (out, registrations) = transform(
        r#"{"meta":{"api-version":"1.1"},"versions":[],"name":"demo","files":[]}"#,
        context,
        8,
    );

    let detail = parse_detail(out.as_bytes()).unwrap();
    assert!(detail.files.is_empty());
    assert!(registrations.is_empty());
}

#[test]
fn test_policy_filters_upstream_files() {
    let policy = policy(|config| {
        config.block_package_types = vec![PackageType::Wheel];
    });
    let context = build_page_context("root/pypi", "demo", policy, Vec::new(), Vec::new(), &BTreeMap::new());

    let (out, registrations) = transform(&upstream_page(), context, 7);

    let detail = parse_detail(out.as_bytes()).unwrap();
    assert_eq!(detail.files.len(), 1);
    assert_eq!(detail.files[0].filename, "demo-2.0.tar.gz");
    assert!(registrations.is_empty());
}

#[test]
fn test_hidden_and_yank_overrides() {
    let overrides = BTreeMap::from([
        (
            "demo-1.0-py3-none-any.whl".to_owned(),
            FileOverride {
                hidden: true,
                yanked: Yanked::No,
            },
        ),
        (
            "demo-2.0-py3-none-any.whl".to_owned(),
            FileOverride {
                hidden: false,
                yanked: Yanked::Reason("bad build".to_owned()),
            },
        ),
        (
            "demo-2.0.tar.gz".to_owned(),
            FileOverride {
                hidden: false,
                yanked: Yanked::Yes,
            },
        ),
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
    let reasonless = detail
        .files
        .iter()
        .find(|file| file.filename == "demo-2.0.tar.gz")
        .unwrap();
    assert_eq!(reasonless.yanked, Yanked::Yes);
}

#[test]
fn test_a_hidden_file_keeps_its_yank_when_it_returns() {
    let overrides = BTreeMap::from([(
        "demo-2.0.tar.gz".to_owned(),
        FileOverride {
            hidden: false,
            yanked: Yanked::Reason("CVE-2026-1234".to_owned()),
        },
    )]);
    let context = page_context("root/pypi", Vec::new(), Vec::new(), &overrides);
    let (out, _) = transform(&upstream_page(), context, 2);
    let detail = parse_detail(out.as_bytes()).unwrap();
    let file = detail
        .files
        .iter()
        .find(|file| file.filename == "demo-2.0.tar.gz")
        .unwrap();
    assert_eq!(file.yanked, Yanked::Reason("CVE-2026-1234".to_owned()));
}

#[test]
fn test_quarantined_project_streams_without_files() {
    let page = r#"{"meta":{"api-version":"1.4"},
        "project-status":{"status":"quarantined","reason":"malware"},
        "name":"demo","versions":["1.0"],"files":[
        {"filename":"demo-1.0-py3-none-any.whl","size":11,"url":"https://up/demo-1.0-py3-none-any.whl",
         "hashes":{"sha256":"aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11"}}
    ]}"#;
    let (out, registrations) = transform(page, plain_context(), 5);
    let detail = parse_detail(out.as_bytes()).unwrap();
    assert_eq!(detail.meta.status(), crate::ProjectStatus::Quarantined);
    assert!(detail.files.is_empty());
    assert!(registrations.is_empty());
}

#[test]
fn test_seeded_legacy_quarantine_withholds_files_when_meta_follows_files() {
    let page = r#"{"name":"demo","versions":["1.0"],"files":[
        {"filename":"demo-1.0-py3-none-any.whl","url":"https://up/demo-1.0-py3-none-any.whl",
         "hashes":{"sha256":"aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11"}}],
        "meta":{"api-version":"1.4","project-status":"quarantined"}}"#;
    let mut transformer = PageTransformer::new(plain_context());
    transformer.seed_project_status(Some("quarantined".to_owned()), Some("malware".to_owned()));
    let out = transformer.push(page.as_bytes()).unwrap();
    let summary = transformer.finish().unwrap();
    let detail = parse_detail(&out).unwrap();
    assert_eq!(
        (detail.meta.status(), detail.meta.project_status_reason.as_deref()),
        (crate::ProjectStatus::Quarantined, Some("malware"))
    );
    let json: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(
        json["project-status"],
        serde_json::json!({"status": "quarantined", "reason": "malware"})
    );
    assert_eq!(json["meta"], serde_json::json!({"api-version": "1.4"}));
    assert!(detail.files.is_empty());
    assert!(summary.registrations.is_empty());
}

#[rstest::rstest]
#[case::missing_status(br#"{"meta":{"api-version":"1.4"},"versions":[],"name":"demo","files":["#)]
#[case::missing_meta(br#"{"project-status":{},"name":"demo","files":["#)]
fn test_files_before_headers_ends_preflight_for_buffering(#[case] page: &[u8]) {
    let mut transformer = PageTransformer::new(plain_context());
    transformer.push(page).unwrap();
    assert!(transformer.header_preflight_done());
    assert!(transformer.files_precede_headers());
}

#[test]
fn test_status_before_files_keeps_streaming() {
    let mut transformer = PageTransformer::new(plain_context());
    transformer
        .push(br#"{"meta":{"api-version":"1.4"},"versions":[],"project-status":{},"name":"demo","files":["#)
        .unwrap();
    assert!(transformer.header_preflight_done());
    assert!(!transformer.files_precede_headers());
    assert!(transformer.headers_known());
}

#[test]
fn test_escapes_and_braces_inside_strings_survive() {
    let page = r#"{"meta":{},"name":"de\"mo}{","versions":[],"files":[
        {"filename":"a{1}-1.0.whl","url":"https://up/a\"b[",
         "hashes":{"sha256":"ee55ee55ee55ee55ee55ee55ee55ee55ee55ee55ee55ee55ee55ee55ee55ee55"},"yanked":false}
    ]}"#;
    for chunk in [1, 5] {
        let (out, registrations) = transform(page, plain_context(), chunk);
        let detail = parse_detail(out.as_bytes()).unwrap();
        assert_eq!(
            detail.files[0].url,
            "/root/pypi/files/ee55ee55ee55ee55ee55ee55ee55ee55ee55ee55ee55ee55ee55ee55ee55ee55/a%7B1%7D-1.0.whl"
        );
        assert_eq!(registrations[0].url, "https://up/a\"b[");
    }
}

#[test]
fn test_escaped_files_key_withholds_quarantined_files() {
    // RFC 8259 lets `files` be spelled `files`; the decoded key must still reach quarantine
    // withholding, or an escaped upstream key would leak a quarantined project's files.
    let page = r#"{"m\u0065ta":{},"project-\u0073tatus":{"status":"quarantined"},
        "name":"demo","fi\u006ces":[
        {"filename":"demo-1.0-py3-none-any.whl","url":"https://up/demo-1.0-py3-none-any.whl",
         "hashes":{"sha256":"aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11"}}
    ]}"#;
    for chunk in [1, page.len()] {
        let (out, registrations) = transform(page, plain_context(), chunk);
        let detail = parse_detail(out.as_bytes()).unwrap();
        assert_eq!(
            detail.meta.status(),
            crate::ProjectStatus::Quarantined,
            "chunk size {chunk}"
        );
        assert!(detail.files.is_empty(), "chunk size {chunk}");
        assert!(registrations.is_empty(), "chunk size {chunk}");
        assert!(!out.contains("up/"), "chunk size {chunk}: {out}");
    }
}

#[test]
fn test_escaped_keys_dispatch_like_plain_spellings() {
    let page = r#"{"m\u0065ta":{"api-version":"1.1"},"\u006eame":"demo",
        "v\u0065rsions":["1.0"],"fi\u006ces":[
        {"filename":"demo-1.0-py3-none-any.whl","size":11,"url":"https://up/demo-1.0-py3-none-any.whl",
         "hashes":{"sha256":"aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11"},"yanked":false}
    ]}"#;
    for chunk in [1, 6, page.len()] {
        let (out, summary) = transform_summary(page, plain_context(), chunk);
        let detail = parse_detail(out.as_bytes()).unwrap();
        assert_eq!(summary.name.as_deref(), Some("demo"), "chunk size {chunk}");
        assert_eq!(detail.versions, ["1.0"], "chunk size {chunk}");
        assert_eq!(
            detail.files[0].url,
            "/root/pypi/files/aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11/demo-1.0-py3-none-any.whl",
            "chunk size {chunk}"
        );
        assert_eq!(
            summary.registrations[0].sha256, "aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11",
            "chunk size {chunk}"
        );
    }
}

#[test]
fn test_escaped_keys_that_cannot_spell_a_member_pass_through() {
    for key in [r"fi\tles", r"m\u00e9ta", r"fi\uD83D\uDE00les"] {
        let page = format!(r#"{{"name":"demo","{key}":1,"files":[]}}"#);
        for chunk in [1, page.len()] {
            let (out, _) = transform_summary(&page, plain_context(), chunk);
            assert_eq!(out, page, "chunk size {chunk}, key {key}");
        }
    }
}

#[test]
fn test_chunk_boundaries_preserve_corpus_output_and_summary() {
    for (page, expected_out, expected_name) in [
        (
            r#"{"meta":{"api-version":"1.4"},"name":"démo","versions":["1\u002e0"],"files":[]}"#,
            r#"{"meta":{"api-version":"1.4"},"name":"démo","versions":["1.0"],"files":[]}"#,
            "démo",
        ),
        (
            r#"{"meta":{"api-version":"1.4","extra":{"nested":["}\"["]}},"name":"demo","versions":[],"files":[]}"#,
            r#"{"meta":{"api-version":"1.4"},"name":"demo","versions":[],"files":[]}"#,
            "demo",
        ),
        (
            r#"{"versions-extra":["1\u002e0"],"versions":["2\u002e0"],"name":"demo","files":[]}"#,
            r#"{"versions-extra":["1\u002e0"],"versions":["2.0"],"name":"demo","files":[]}"#,
            "demo",
        ),
    ] {
        for chunk in 1..=page.len().min(32) {
            assert_eq!(
                transform_summary(page, plain_context(), chunk),
                (
                    expected_out.to_owned(),
                    PageSummary {
                        registrations: Vec::new(),
                        name: Some(expected_name.to_owned()),
                        project_status: None,
                        project_status_reason: None,
                    },
                ),
                "chunk size {chunk}"
            );
        }
    }
}

#[test]
fn test_push_into_appends_without_replacing_existing_bytes() {
    let mut transformer = PageTransformer::new(plain_context());
    let mut out = b"prefix:".to_vec();

    transformer
        .push_into(
            br#"{"meta":{"api-version":"1.4"},"versions":[],"name":"demo","files":[]}"#,
            &mut out,
        )
        .unwrap();
    transformer.finish().unwrap();

    assert_eq!(
        out,
        br#"prefix:{"meta":{"api-version":"1.4"},"versions":[],"name":"demo","files":[]}"#
    );
}

#[test]
fn test_versions_after_files_and_empty_files() {
    let page = r#"{"files":[],"versions":["2.0"],"name":"demo"}"#;
    let context = page_context("r", Vec::new(), vec!["1.0".to_owned()], &BTreeMap::new());
    let (out, registrations) = transform(page, context, 1);
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["versions"], serde_json::json!(["1.0", "2.0"]));
    assert_eq!(value["files"].as_array().unwrap().len(), 0);
    assert!(registrations.is_empty());
}

#[test]
fn test_local_files_emitted_into_empty_upstream_array() {
    let page = r#"{"meta":{},"name":"demo","files":[]}"#;
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
    let (out, _) = transform(page, page_context("r", vec![local], Vec::new(), &BTreeMap::new()), 3);
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
        "hashes":{"sha256":"aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11"},"extra":["sig1","sig2"]}]}"#;
    let (out, registrations) = transform(page, plain_context(), 5);
    assert_eq!(registrations.len(), 1);
    assert!(out.contains(
        "/root/pypi/files/aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11/demo-1.0-py3-none-any.whl"
    ));
}

#[test]
fn test_preserves_simple_api_fields_during_streaming() {
    let page = r#"{"meta":{"api-version":"1.4"},
        "project-status":{"status":"archived","reason":"read only"},
        "name":"demo","versions":["1.0"],"files":[
        {"filename":"demo-1.0-py3-none-any.whl","url":"https://up/demo-1.0-py3-none-any.whl",
         "hashes":{"sha256":"aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11"},"size":10,"upload-time":"2024-01-01T00:00:00Z",
         "core-metadata":{"sha256":"bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22"},"dist-info-metadata":{"sha256":"bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22"},
         "gpg-sig":false,"provenance":"https://up/demo-1.0-py3-none-any.whl.provenance"}
    ]}"#;
    for chunk in [1, 11, 4096] {
        let (out, registrations) = transform(page, plain_context(), chunk);
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
                    "bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22".to_owned(),
                )])),
                &CoreMetadata::Hashes(std::collections::BTreeMap::from([(
                    "sha256".to_owned(),
                    "bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22".to_owned(),
                )])),
                None,
                &Provenance::Url("https://up/demo-1.0-py3-none-any.whl.provenance".to_owned()),
            ),
            "chunk size {chunk}"
        );
        assert_eq!(
            registrations[0].provenance.as_deref(),
            Some("https://up/demo-1.0-py3-none-any.whl.provenance")
        );
    }
}

#[test]
fn test_proxy_mode_rewrites_a_secure_provenance_url() {
    let page = r#"{"meta":{},"name":"demo","files":[{"filename":"demo-1.0-py3-none-any.whl",
        "url":"https://up/demo.whl","hashes":{"sha256":"aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11"},
        "provenance":"https://up/demo.whl.provenance"}]}"#;
    let mut context = plain_context();
    context.policy = policy(|config| config.upstream_attestations = RemoteMetadataMode::Proxy);

    let (out, registrations) = transform(page, context, 5);
    let detail = parse_detail(out.as_bytes()).unwrap();

    assert_eq!(
        detail.files[0].provenance,
        Provenance::Url("/root/pypi/files/aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11/demo-1.0-py3-none-any.whl.provenance".to_owned())
    );
    assert_eq!(
        registrations[0].provenance.as_deref(),
        Some("https://up/demo.whl.provenance")
    );
}

#[test]
fn test_streaming_drops_an_insecure_provenance_url() {
    let page = r#"{"meta":{},"name":"demo","files":[{"filename":"demo-1.0-py3-none-any.whl",
        "url":"https://up/demo.whl","hashes":{"sha256":"aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11"},
        "provenance":"http://up/demo.whl.provenance"}]}"#;

    let (out, registrations) = transform(page, plain_context(), 5);
    let detail = parse_detail(out.as_bytes()).unwrap();

    assert_eq!(detail.files[0].provenance, Provenance::Absent);
    assert_eq!(registrations[0].provenance, None);
}

#[test]
fn test_project_status_streaming_handles_escaped_and_unknown_values() {
    let page = r#"{"meta":{"api-version":"1.4"},
        "versions":[],"project-status":{"status":"archived","reason":"read \"only\"","extra":[{"ignored":"yes"}]},
        "name":"demo","files":[]}"#;
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
fn test_streaming_rejects_unknown_project_status() {
    let mut transformer = PageTransformer::new(plain_context());
    let result = transformer
        .push(br#"{"meta":{"api-version":"1.4"},"versions":[],"project-status":{"status":"frozen"},"name":"demo","files":[]}"#);
    assert!(matches!(result, Err(crate::stream::TransformError::Simple(_))));
}

#[test]
fn test_streaming_rejects_unsupported_major_api_version() {
    let mut transformer = PageTransformer::new(plain_context());
    let result = transformer.push(br#"{"meta":{"api-version":"2.0"},"name":"demo","files":[]}"#);
    assert!(result.is_err());
}

#[test]
fn test_escaped_version_strings_merge() {
    let page = r#"{"meta":{},"name":"demo","versions":["1\u002e0","2.0"],"files":[]}"#;
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
        &BTreeMap::new(),
    );
    let (out, _) = transform(r#"{"meta":{},"name":"demo","files":[]}"#, context, 4096);
    let detail = parse_detail(out.as_bytes()).unwrap();
    assert_eq!(detail.files.len(), 2);
}

fn based_context(base: &str) -> PageContext {
    let mut context = plain_context();
    context.base = Some(url::Url::parse(base).unwrap());
    context
}

fn one_file_page(url: &str, hashes: &str) -> String {
    format!(
        r#"{{"meta":{{}},"name":"demo","files":[{{"filename":"demo-1.0-py3-none-any.whl","url":"{url}","hashes":{hashes}}}]}}"#
    )
}

#[rstest]
#[case::relative(
    "demo-1.0-py3-none-any.whl",
    r#"{"sha256":"aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11"}"#,
    "/root/pypi/files/aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11/demo-1.0-py3-none-any.whl",
    Some("https://mirror.test/simple/demo/demo-1.0-py3-none-any.whl")
)]
#[case::root_relative(
    "/packages/demo-1.0-py3-none-any.whl",
    r#"{"sha256":"aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11"}"#,
    "/root/pypi/files/aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11/demo-1.0-py3-none-any.whl",
    Some("https://mirror.test/packages/demo-1.0-py3-none-any.whl")
)]
#[case::protocol_relative(
    "//cdn.test/demo-1.0-py3-none-any.whl",
    r#"{"sha256":"aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11"}"#,
    "/root/pypi/files/aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11/demo-1.0-py3-none-any.whl",
    Some("https://cdn.test/demo-1.0-py3-none-any.whl")
)]
#[case::absolute(
    "https://files.test/demo-1.0-py3-none-any.whl",
    "{}",
    "https://files.test/demo-1.0-py3-none-any.whl",
    None
)]
fn test_resolves_file_url_against_the_response_url(
    #[case] source_url: &str,
    #[case] hashes: &str,
    #[case] expected_url: &str,
    #[case] expected_registration: Option<&str>,
) {
    let (out, registrations) = transform(
        &one_file_page(source_url, hashes),
        based_context("https://mirror.test/simple/demo/"),
        5,
    );
    let detail = parse_detail(out.as_bytes()).unwrap();
    assert_eq!(
        (
            detail.files[0].url.as_str(),
            registrations
                .iter()
                .map(|registration| registration.url.as_str())
                .collect::<Vec<_>>(),
        ),
        (expected_url, expected_registration.into_iter().collect::<Vec<_>>())
    );
}

#[rstest]
#[case::route_prefix("/root/pypi/files/releases/demo-1.0-py3-none-any.whl")]
#[case::different_digest(
    "/root/pypi/files/bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22/demo-1.0-py3-none-any.whl"
)]
#[case::different_filename(
    "/root/pypi/files/aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11/other.whl"
)]
#[case::extra_path_segment(
    "/root/pypi/files/aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11/releases/demo-1.0-py3-none-any.whl"
)]
fn test_incomplete_local_urls_resolve_register_and_rewrite(#[case] source_url: &str) {
    let (out, registrations) = transform(
        &one_file_page(
            source_url,
            r#"{"sha256":"aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11"}"#,
        ),
        based_context("https://mirror.test/simple/demo/"),
        6,
    );
    let detail = parse_detail(out.as_bytes()).unwrap();
    assert_eq!(
        (detail.files[0].url.as_str(), registrations),
        (
            "/root/pypi/files/aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11/demo-1.0-py3-none-any.whl",
            vec![Registration {
                filename: "demo-1.0-py3-none-any.whl".to_owned(),
                sha256: "aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11".to_owned(),
                url: format!("https://mirror.test{source_url}"),
                size: None,
                metadata: None,
                provenance: None,
            }],
        )
    );
}

#[test]
fn test_complete_legacy_record_url_passes_through_unregistered() {
    let source_url =
        "/root/pypi/files/aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11/demo-1.0-py3-none-any.whl";
    let (out, registrations) = transform(
        &one_file_page(
            source_url,
            r#"{"sha256":"aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11"}"#,
        ),
        based_context("https://mirror.test/simple/demo/"),
        6,
    );
    let detail = parse_detail(out.as_bytes()).unwrap();
    assert_eq!((detail.files[0].url.as_str(), registrations), (source_url, Vec::new()));
}

#[test]
fn test_streaming_drops_gpg_sig_on_a_legacy_local_record() {
    let page = r#"{"meta":{},"name":"demo","files":[{"filename":"demo-1.0-py3-none-any.whl",
        "url":"/root/pypi/files/aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11/demo-1.0-py3-none-any.whl","hashes":{"sha256":"aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11"},"gpg-sig":true}]}"#;
    let (out, _) = transform(page, plain_context(), 8);
    let detail = parse_detail(out.as_bytes()).unwrap();
    assert_eq!(detail.files[0].gpg_sig, None);
}

#[test]
fn test_streaming_keeps_gpg_sig_when_the_url_stays_upstream() {
    let page = r#"{"meta":{},"name":"demo","files":[{"filename":"demo-1.0.tar.gz",
        "url":"https://up/demo-1.0.tar.gz","hashes":{},"gpg-sig":true}]}"#;
    let (out, _) = transform(page, plain_context(), 7);
    let detail = parse_detail(out.as_bytes()).unwrap();
    assert_eq!(detail.files[0].url, "https://up/demo-1.0.tar.gz");
    assert_eq!(detail.files[0].gpg_sig, Some(true));
}

#[test]
fn test_legacy_record_after_a_rewritten_file_keeps_separators() {
    let page = r#"{"meta":{},"name":"demo","files":[
        {"filename":"demo-1.0-py3-none-any.whl","url":"https://up/demo-1.0-py3-none-any.whl",
         "hashes":{"sha256":"aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11"}},
        {"filename":"demo-2.0-py3-none-any.whl","url":"/root/pypi/files/bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22/demo-2.0-py3-none-any.whl",
         "hashes":{"sha256":"bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22"}}]}"#;
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

#[test]
fn test_trailing_bracket_after_the_root_is_an_error() {
    for suffix in ["}", "]", "}]", "}}"] {
        let mut transformer = PageTransformer::new(plain_context());
        transformer
            .push(format!(r#"{{"name":"demo","files":[]}}{suffix}"#).as_bytes())
            .unwrap();
        assert!(transformer.finish().is_err(), "suffix {suffix:?}");
    }
}

#[test]
fn test_trailing_whitespace_after_the_root_is_clean() {
    let mut transformer = PageTransformer::new(plain_context());
    transformer.push(b"{\"name\":\"demo\",\"files\":[]}\n \t").unwrap();
    assert!(transformer.finish().is_ok());
}

#[test]
fn test_non_string_top_level_name_keeps_rewriting_files() {
    // A null, numeric, or object `name` must not derail key tracking and disable rewriting.
    for name in ["null", "42", r#"{"nested":true}"#] {
        let page = format!(
            r#"{{"name":{name},"files":[
                {{"filename":"demo-1.0-py3-none-any.whl","url":"https://up/demo-1.0-py3-none-any.whl",
                 "hashes":{{"sha256":"aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11"}}}}]}}"#
        );
        for chunk in [1, 5, 4096] {
            let (out, registrations) = transform(&page, plain_context(), chunk);
            assert!(
                out.contains("/root/pypi/files/aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11/demo-1.0-py3-none-any.whl"),
                "name {name} chunk {chunk}"
            );
            assert_eq!(registrations.len(), 1, "name {name} chunk {chunk}");
        }
    }
}

#[test]
fn test_metadata_sibling_lands_on_the_path_not_the_query() {
    let page = r#"{"name":"demo","files":[{"filename":"demo-1.0-py3-none-any.whl",
        "url":"https://files.test/demo-1.0-py3-none-any.whl?token=abc",
        "hashes":{"sha256":"aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11"},"core-metadata":{"sha256":"bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22"}}]}"#;
    let (_, registrations) = transform(page, plain_context(), 6);
    assert_eq!(
        registrations[0].metadata,
        Some((
            "https://files.test/demo-1.0-py3-none-any.whl.metadata?token=abc".to_owned(),
            "bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22".to_owned()
        ))
    );
}

#[test]
fn test_local_file_hidden_override_is_dropped_like_the_buffered_path() {
    let overrides = BTreeMap::from([(
        "demo-1.0-py3-none-any.whl".to_owned(),
        FileOverride {
            hidden: true,
            yanked: Yanked::No,
        },
    )]);
    let context = page_context(
        "root/pypi",
        vec![local_wheel("demo-1.0-py3-none-any.whl")],
        Vec::new(),
        &overrides,
    );
    let (out, _) = transform(r#"{"meta":{},"name":"demo","files":[]}"#, context, 4096);
    let detail = parse_detail(out.as_bytes()).unwrap();
    assert!(detail.files.is_empty());
}

#[test]
fn test_local_file_yank_override_is_applied_like_the_buffered_path() {
    let overrides = BTreeMap::from([(
        "demo-1.0-py3-none-any.whl".to_owned(),
        FileOverride {
            hidden: false,
            yanked: Yanked::Reason("bad build".to_owned()),
        },
    )]);
    let context = page_context(
        "root/pypi",
        vec![local_wheel("demo-1.0-py3-none-any.whl")],
        Vec::new(),
        &overrides,
    );
    let (out, _) = transform(r#"{"meta":{},"name":"demo","files":[]}"#, context, 4096);
    let detail = parse_detail(out.as_bytes()).unwrap();
    assert_eq!(detail.files[0].yanked, Yanked::Reason("bad build".to_owned()));
}

#[test]
fn test_rejects_a_page_past_the_byte_limit() {
    let mut transformer = PageTransformer::new(plain_context());
    // Whitespace is a valid JSON lead-in that copies straight through, so the byte guard - not a
    // parse error - is what has to stop the oversized page. 65 MiB in 1 MiB pushes clears the 64 MiB
    // cap; a bounded loop fails cleanly if the guard is dropped, where an unbounded one would hang.
    let chunk = vec![b' '; 1024 * 1024];
    let mut result = Ok(Vec::new());
    for _ in 0..65 {
        result = transformer.push(&chunk);
    }
    let err = result.unwrap_err();
    assert_eq!(err.to_string(), "upstream page exceeds the size or file-count limit");
    assert!(matches!(err, TransformError::TooLarge));
}

fn stream_result(page: impl AsRef<[u8]>, chunk: usize) -> Result<PageSummary, TransformError> {
    let page = page.as_ref();
    let mut transformer = PageTransformer::new(plain_context());
    let mut out = Vec::new();
    for piece in page.chunks(chunk) {
        transformer.push_into(piece, &mut out)?;
    }
    transformer.finish()
}

#[test]
fn test_non_object_root_is_rejected() {
    for page in [
        "123",
        "12.5",
        "-0",
        r#""demo""#,
        "true",
        "false",
        "null",
        "[]",
        r#"["a"]"#,
    ] {
        for chunk in [1, page.len()] {
            assert!(
                matches!(stream_result(page, chunk), Err(TransformError::Malformed)),
                "page {page} chunk {chunk}"
            );
        }
    }
}

#[test]
fn test_malformed_top_level_punctuation_is_rejected() {
    // A missing value after `:` keeps balanced depth and finishes clean through the structural
    // lexer; only the grammar guard rejects it before it can be cached.
    let page = r#"{"files":[],"unknown":,}"#;
    for chunk in [1, page.len()] {
        assert!(
            matches!(stream_result(page, chunk), Err(TransformError::Malformed)),
            "chunk {chunk}"
        );
    }
}

#[rstest]
#[case::space(' ', true)]
#[case::horizontal_tab('\t', true)]
#[case::line_feed('\n', true)]
#[case::carriage_return('\r', true)]
#[case::vertical_tab('\u{000b}', false)]
#[case::form_feed('\u{000c}', false)]
fn test_streaming_and_buffered_parsers_agree_on_json_whitespace(#[case] whitespace: char, #[case] accepted: bool) {
    let page = format!(r#"{{"meta":{{"api-version":"1.4"}},"name":"demo"{whitespace},"versions":[],"files":[]}}"#);
    assert_eq!(parse_detail(page.as_bytes()).is_ok(), accepted);
    for chunk in [1, page.len()] {
        assert_eq!(stream_result(&page, chunk).is_ok(), accepted, "chunk {chunk}");
    }
}

#[test]
fn test_non_hex_unicode_escape_in_a_key_is_rejected() {
    // `\u` must be followed by four hex digits; the structural lexer passes the malformed key
    // through, so the grammar guard is what fails the body.
    let page = r#"{"na\uzzzze":1,"files":[]}"#;
    for chunk in [1, page.len()] {
        assert!(
            matches!(stream_result(page, chunk), Err(TransformError::Malformed)),
            "chunk {chunk}"
        );
    }
}

#[rstest]
#[case::valid_pair(br#""demo\uD83D\uDE00""#, true)]
#[case::lone_high(br#""demo\uD800""#, false)]
#[case::lone_low(br#""demo\uDC00""#, false)]
#[case::missing_low_escape(br#""demo\uD800\x""#, false)]
#[case::non_low_pair(br#""demo\uD800\u0041""#, false)]
fn test_streaming_and_buffered_parsers_agree_on_surrogate_validity(#[case] name: &[u8], #[case] accepted: bool) {
    let page = [
        br#"{"meta":{"api-version":"1.4"},"name":"#,
        name,
        br#", "versions":[],"files":[]}"#,
    ]
    .concat();
    assert_streaming_matches_buffered(&page, accepted);
}

#[rstest]
#[case::valid_two_byte(b"\"\xC2\x80\"", true)]
#[case::valid_three_byte_lower(b"\"\xE0\xA0\x80\"", true)]
#[case::valid_three_byte(b"\"\xE2\x82\xAC\"", true)]
#[case::valid_four_byte_lower(b"\"\xF0\x90\x80\x80\"", true)]
#[case::valid_four_byte(b"\"\xF1\x80\x80\x80\"", true)]
#[case::valid_four_byte_upper(b"\"\xF4\x8F\xBF\xBF\"", true)]
#[case::invalid_lead(b"\"demo\xFF\"", false)]
#[case::invalid_continuation(b"\"demo\xC2A\"", false)]
#[case::overlong(b"\"demo\xC0\xAF\"", false)]
#[case::raw_surrogate(b"\"demo\xED\xA0\x80\"", false)]
fn test_streaming_validator_accepts_only_valid_utf8(#[case] value: &[u8], #[case] accepted: bool) {
    let page = [
        br#"{"meta":{"api-version":"1.4"},"name":"demo","versions":[],"extra":"#,
        value,
        br#", "files":[]}"#,
    ]
    .concat();
    for chunk in [1, page.len()] {
        assert_eq!(stream_result(&page, chunk).is_ok(), accepted, "chunk {chunk}");
    }
}

fn assert_streaming_matches_buffered(page: &[u8], accepted: bool) {
    for chunk in [1, page.len()] {
        assert_eq!(
            (stream_result(page, chunk).is_ok(), parse_detail(page).is_ok()),
            (accepted, accepted),
            "chunk {chunk}"
        );
    }
}

#[test]
fn test_grammar_guard_accepts_every_json_scalar_shape() {
    // The number, string, and literal DFAs must clear valid inputs, or the guard would reject good
    // pages: signs, fractions, exponents, escapes, and the three keywords all appear here, carried
    // by an unrecognized member the structural lexer copies through untouched.
    let page = r#"{"meta":{"api-version":"1.4"},"name":"demo","versions":["1.0"],
        "extra":[null,true,false,0,-0,0e1,12,3.14,-1.5e10,1E+3,-0.0e-2,"a\t\f\u000cbé","",{}],
        "files":[{"filename":"demo-1.0-py3-none-any.whl","size":11,"url":"https://up/demo.whl",
         "hashes":{"sha256":"aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11"},"gpg-sig":true,"yanked":false}]}"#;
    for chunk in [1, 5, page.len()] {
        assert!(stream_result(page, chunk).is_ok(), "chunk {chunk}");
    }
}

#[test]
fn test_grammar_guard_rejects_every_structural_violation() {
    for page in [
        r#"{"a":,}"#,
        r"{,}",
        r#"{"a":1,2}"#,
        r#"{"a"1}"#,
        r#"{"a":1 2}"#,
        r#"{"a":[0}]"#,
        r#"{"a":"\q"}"#,
        "{\"a\":\"x\ny\"}",
        r#"{"a":-}"#,
        r#"{"a":1.}"#,
        r#"{"a":1e}"#,
        r#"{"a":1e+}"#,
        r#"{"a":nul}"#,
    ] {
        for chunk in [1, page.len()] {
            assert!(
                matches!(stream_result(page, chunk), Err(TransformError::Malformed)),
                "page {page:?} chunk {chunk}"
            );
        }
    }
}

#[test]
fn test_incomplete_page_is_rejected() {
    for page in ["", "   "] {
        let mut transformer = PageTransformer::new(plain_context());
        transformer.push(page.as_bytes()).unwrap();
        assert!(
            matches!(transformer.finish(), Err(TransformError::Malformed)),
            "page {page:?}"
        );
    }
}

#[test]
fn test_rejects_a_page_with_too_many_files() {
    // One element past the 500_000 cap, while the whole page stays far under the byte cap, so the
    // file-count guard is the one that must fire.
    let files = 500_001;
    let mut page = String::with_capacity(files * 27 + 16);
    page.push_str(r#"{"files":["#);
    for index in 0..files {
        if index > 0 {
            page.push(',');
        }
        page.push_str(r#"{"filename":"a","url":"b"}"#);
    }
    page.push_str("]}");
    let mut transformer = PageTransformer::new(plain_context());
    assert!(matches!(
        transformer.push(page.as_bytes()).unwrap_err(),
        TransformError::TooLarge
    ));
}

#[rstest]
#[case::no_versions(r#"{"meta":{"api-version":"1.1"},"name":"demo","files":[]}"#)]
#[case::no_file_size(
    r#"{"meta":{"api-version":"1.1"},"name":"demo","versions":["1.0"],
        "files":[{"filename":"demo-1.0.tar.gz","url":"https://up/demo-1.0.tar.gz"}]}"#
)]
#[case::duplicate_version(r#"{"meta":{"api-version":"1.1"},"name":"demo","versions":["1.0","1.0"],"files":[]}"#)]
fn test_streaming_rejects_an_incomplete_pep700_page(#[case] page: &str) {
    let mut transformer = PageTransformer::new(plain_context());
    let mut out = Vec::new();

    let outcome = transformer
        .push_into(page.as_bytes(), &mut out)
        .and_then(|()| transformer.finish().map(|_| ()));

    assert!(matches!(outcome, Err(TransformError::Simple(_))), "{outcome:?}");
}

#[test]
fn test_streaming_keeps_a_size_less_file_on_a_pre_pep700_page() {
    let page = r#"{"meta":{"api-version":"1.0"},"name":"demo","files":[
        {"filename":"demo-1.0.tar.gz","url":"https://up/demo-1.0.tar.gz","hashes":{"sha256":"aa11"}}]}"#;

    let (out, _) = transform(page, plain_context(), 9);

    let detail = parse_detail(out.as_bytes()).unwrap();
    assert_eq!((detail.files.len(), detail.files[0].size), (1, None));
}
