use std::collections::BTreeMap;
use std::error::Error as _;

use crate::pypi::{
    CoreMetadata, File, Meta, ProjectDetail, ProjectList, ProjectListEntry, ProjectStatus, Provenance, SimpleError,
    Yanked, parse_index, render_detail_html, render_index_html, render_legacy_json, to_json,
};

fn sha256(value: &str) -> BTreeMap<String, String> {
    BTreeMap::from([("sha256".to_owned(), value.to_owned())])
}

/// A detail whose three files together exercise every field and enum variant, plus HTML escaping
/// of `&`, `<`, `>` (text) and `&`, `<`, `>`, `"` (attributes).
fn sample_detail() -> ProjectDetail {
    ProjectDetail {
        meta: Meta {
            api_version: crate::pypi::API_VERSION,
            project_status: Some("active".to_owned()),
            project_status_reason: Some("available".to_owned()),
        },
        name: "proj&<>".to_owned(),
        versions: vec!["1.0".to_owned(), "2.0".to_owned()],
        files: vec![
            File {
                filename: "proj&<>-2.0-py3-none-any.whl".to_owned(),
                url: "https://files.example/a?b=1&c=2".to_owned(),
                hashes: sha256("aaaa"),
                requires_python: Some(">=3.8,<4".to_owned()),
                size: Some(1234),
                upload_time: Some("2024-03-24T00:00:00.000000Z".to_owned()),
                yanked: Yanked::No,
                core_metadata: CoreMetadata::Hashes(sha256("bbbb")),
                dist_info_metadata: CoreMetadata::Hashes(sha256("bbbb")),
                gpg_sig: Some(true),
                provenance: Provenance::Url("https://files.example/a.provenance".to_owned()),
            },
            File {
                filename: "proj-1.5.tar.gz".to_owned(),
                url: "https://files.example/q\"uote".to_owned(),
                hashes: BTreeMap::new(),
                requires_python: None,
                size: None,
                upload_time: None,
                yanked: Yanked::Reason("broken build".to_owned()),
                core_metadata: CoreMetadata::Available,
                dist_info_metadata: CoreMetadata::Available,
                gpg_sig: Some(false),
                provenance: Provenance::Absent,
            },
            File {
                filename: "proj-1.0-py3-none-any.whl".to_owned(),
                url: "https://files.example/c.whl".to_owned(),
                hashes: sha256("cccc"),
                requires_python: None,
                size: Some(9),
                upload_time: None,
                yanked: Yanked::Yes,
                core_metadata: CoreMetadata::Absent,
                dist_info_metadata: CoreMetadata::Absent,
                gpg_sig: None,
                provenance: Provenance::None,
            },
        ],
    }
}

fn sample_list() -> ProjectList {
    ProjectList {
        meta: Meta::default(),
        projects: vec![
            ProjectListEntry {
                name: "Flask".to_owned(),
            },
            ProjectListEntry {
                name: "zope.interface".to_owned(),
            },
            ProjectListEntry {
                name: "a&<>".to_owned(),
            },
        ],
    }
}

#[test]
fn test_detail_html_snapshot() {
    insta::assert_snapshot!("detail_html", render_detail_html(&sample_detail()));
}

#[test]
fn test_detail_json_snapshot() {
    insta::assert_snapshot!("detail_json", to_json(&sample_detail()));
}

#[test]
fn test_legacy_project_json_maps_simple_fields() {
    let detail = sample_detail();
    let legacy: serde_json::Value = serde_json::from_str(&render_legacy_json(&detail, None).unwrap()).unwrap();

    assert_eq!(
        legacy["info"],
        serde_json::json!({
            "author": "",
            "author_email": "",
            "bugtrack_url": null,
            "classifiers": [],
            "description": "",
            "description_content_type": null,
            "docs_url": null,
            "download_url": "",
            "downloads": {"last_day": -1, "last_month": -1, "last_week": -1},
            "dynamic": [],
            "home_page": "",
            "keywords": "",
            "license": "",
            "license_expression": null,
            "license_files": null,
            "maintainer": "",
            "maintainer_email": "",
            "name": "proj&<>",
            "package_url": "",
            "platform": null,
            "project_url": "",
            "project_urls": {},
            "provides_extra": [],
            "release_url": "",
            "requires_dist": [],
            "requires_python": ">=3.8,<4",
            "summary": "",
            "version": "2.0",
            "yanked": false,
            "yanked_reason": null
        })
    );
    assert_eq!(legacy["last_serial"], 0);
    assert_eq!(legacy["vulnerabilities"], serde_json::json!([]));
    assert_eq!(
        legacy["ownership"],
        serde_json::json!({"roles": [], "organization": null})
    );
    assert_eq!(legacy["urls"], legacy["releases"]["2.0"]);
    assert_eq!(
        legacy["urls"][0],
        serde_json::json!({
            "comment_text": "",
            "digests": {"sha256": "aaaa"},
            "downloads": -1,
            "filename": "proj&<>-2.0-py3-none-any.whl",
            "has_sig": true,
            "md5_digest": null,
            "packagetype": "bdist_wheel",
            "python_version": "py3",
            "requires_python": ">=3.8,<4",
            "size": 1234,
            "upload_time": "2024-03-24T00:00:00",
            "upload_time_iso_8601": "2024-03-24T00:00:00.000000Z",
            "url": "https://files.example/a?b=1&c=2",
            "yanked": false,
            "yanked_reason": null
        })
    );
    assert_eq!(
        legacy["releases"]["1.5"][0],
        serde_json::json!({
            "comment_text": "",
            "digests": {},
            "downloads": -1,
            "filename": "proj-1.5.tar.gz",
            "has_sig": false,
            "md5_digest": null,
            "packagetype": "sdist",
            "python_version": "source",
            "requires_python": null,
            "size": null,
            "upload_time": null,
            "upload_time_iso_8601": null,
            "url": "https://files.example/q\"uote",
            "yanked": true,
            "yanked_reason": "broken build"
        })
    );
}

#[test]
fn test_legacy_release_json_omits_releases_and_matches_equivalent_version() {
    let detail = sample_detail();
    let legacy: serde_json::Value = serde_json::from_str(&render_legacy_json(&detail, Some("1.0.0")).unwrap()).unwrap();

    assert_eq!(legacy.get("releases"), None);
    assert_eq!(legacy["info"]["version"], "1.0");
    assert_eq!(legacy["info"]["yanked"], true);
    assert_eq!(legacy["urls"][0]["filename"], "proj-1.0-py3-none-any.whl");
    assert_eq!(legacy["urls"][0]["yanked_reason"], serde_json::Value::Null);
}

#[test]
fn test_legacy_release_json_rejects_unknown_version() {
    assert_eq!(render_legacy_json(&sample_detail(), Some("9.9")), None);
}

#[test]
fn test_legacy_release_json_resolves_filename_only_version() {
    let mut detail = sample_detail();
    detail.versions = vec!["not-a-version".to_owned()];

    let legacy: serde_json::Value = serde_json::from_str(&render_legacy_json(&detail, Some("1.5")).unwrap()).unwrap();

    assert_eq!(legacy["info"]["version"], "1.5");
    assert_eq!(legacy["urls"][0]["filename"], "proj-1.5.tar.gz");
}

#[test]
fn test_legacy_project_json_uses_advertised_version_when_no_files() {
    let detail = ProjectDetail {
        meta: Meta::default(),
        name: "empty-release".to_owned(),
        versions: vec!["1.0".to_owned()],
        files: Vec::new(),
    };
    let legacy: serde_json::Value = serde_json::from_str(&render_legacy_json(&detail, None).unwrap()).unwrap();

    assert_eq!(
        (
            legacy["info"]["version"].as_str(),
            &legacy["urls"],
            &legacy["releases"]["1.0"],
        ),
        (Some("1.0"), &serde_json::json!([]), &serde_json::json!([]))
    );
}

#[test]
fn test_legacy_project_json_maps_legacy_filename_shapes() {
    let detail = ProjectDetail {
        meta: Meta::default(),
        name: "proj".to_owned(),
        versions: vec![
            "2.1".to_owned(),
            "1.2".to_owned(),
            "1.1".to_owned(),
            "0.9".to_owned(),
            "not-a-version".to_owned(),
        ],
        files: [
            "proj-2.1-1-py3-none-any.whl",
            "proj-1.2.whl",
            "proj-1.1.zip",
            "proj-0.9-py3-none-any.egg",
            "README",
        ]
        .into_iter()
        .map(|filename| File {
            filename: filename.to_owned(),
            url: format!("https://files.example/{filename}"),
            hashes: BTreeMap::new(),
            requires_python: None,
            size: None,
            upload_time: None,
            yanked: Yanked::No,
            core_metadata: CoreMetadata::Absent,
            dist_info_metadata: CoreMetadata::Absent,
            gpg_sig: None,
            provenance: Provenance::Absent,
        })
        .collect(),
    };
    let legacy: serde_json::Value = serde_json::from_str(&render_legacy_json(&detail, None).unwrap()).unwrap();

    assert_eq!(legacy["releases"]["2.1"][0]["python_version"], "py3");
    assert_eq!(legacy["releases"]["1.2"][0]["python_version"], "source");
    assert_eq!(legacy["releases"]["1.1"][0]["packagetype"], "sdist");
    assert_eq!(legacy["releases"]["0.9"][0]["packagetype"], "bdist_egg");
    assert_eq!(legacy["releases"]["not-a-version"], serde_json::json!([]));
    assert_eq!(legacy["releases"].get("README"), None);
}

#[test]
fn test_legacy_project_json_handles_empty_project() {
    let detail = ProjectDetail {
        meta: Meta::default(),
        name: "empty".to_owned(),
        versions: Vec::new(),
        files: Vec::new(),
    };
    let legacy: serde_json::Value = serde_json::from_str(&render_legacy_json(&detail, None).unwrap()).unwrap();

    assert_eq!(
        (
            legacy["info"]["version"].as_str(),
            legacy["info"]["requires_python"].as_str(),
            legacy["info"]["yanked"].as_bool(),
            &legacy["info"]["yanked_reason"],
            &legacy["urls"],
            &legacy["releases"],
        ),
        (
            Some(""),
            None,
            Some(false),
            &serde_json::Value::Null,
            &serde_json::json!([]),
            &serde_json::json!({}),
        )
    );
}

#[test]
fn test_index_html_snapshot() {
    insta::assert_snapshot!("index_html", render_index_html(&sample_list()));
}

#[test]
fn test_index_json_snapshot() {
    insta::assert_snapshot!("index_json", to_json(&sample_list()));
}

#[test]
fn test_parse_index_json() {
    let parsed = parse_index(
        br#"{
            "meta": {"api-version": "1.4"},
            "projects": [{"name": "Flask"}, {"name": "zope.interface"}]
        }"#,
    )
    .unwrap();
    assert_eq!(
        parsed,
        ProjectList {
            meta: Meta::default(),
            projects: vec![
                ProjectListEntry {
                    name: "Flask".to_owned(),
                },
                ProjectListEntry {
                    name: "zope.interface".to_owned(),
                },
            ],
        }
    );
}

#[test]
fn test_parse_index_rejects_unsupported_major_api_version() {
    let err = parse_index(br#"{"meta":{"api-version":"2.0"},"projects":[]}"#).unwrap_err();
    assert!(matches!(err, SimpleError::UnsupportedApiVersion(version) if version == "2.0"));
}

#[test]
fn test_render_detail_html_omits_non_sha256_metadata_hash_attr() {
    let mut hashes = BTreeMap::new();
    hashes.insert("sha512".to_owned(), "abc".to_owned());
    let html = render_detail_html(&ProjectDetail {
        meta: Meta::default(),
        name: "proj".to_owned(),
        versions: vec!["1.0".to_owned()],
        files: vec![File {
            filename: "proj-1.0-py3-none-any.whl".to_owned(),
            url: "https://files.example/proj-1.0-py3-none-any.whl".to_owned(),
            hashes: BTreeMap::new(),
            requires_python: None,
            size: None,
            upload_time: None,
            yanked: Yanked::No,
            core_metadata: CoreMetadata::Hashes(hashes.clone()),
            dist_info_metadata: CoreMetadata::Hashes(hashes),
            gpg_sig: None,
            provenance: Provenance::Absent,
        }],
    });

    assert!(!html.contains("data-core-metadata"));
    assert!(!html.contains("data-dist-info-metadata"));
}

#[test]
fn test_parse_detail_roundtrips_serialized_model() {
    let detail = sample_detail();
    let parsed = crate::pypi::parse_detail(to_json(&detail).as_bytes()).unwrap();
    assert_eq!(parsed.meta, detail.meta);
    assert_eq!(parsed.name, detail.name);
    assert_eq!(parsed.versions, detail.versions);
    assert_eq!(parsed.files, detail.files);
}

#[test]
fn test_parse_detail_minimal() {
    let parsed = crate::pypi::parse_detail(b"{\"name\":\"x\"}").unwrap();
    assert_eq!(parsed.meta, Meta::default());
    assert_eq!(parsed.name, "x");
    assert!(parsed.versions.is_empty());
    assert!(parsed.files.is_empty());
}

#[test]
fn test_parse_detail_reads_both_metadata_spellings() {
    let json = r#"{"name":"x","files":[{"filename":"x-1.whl","url":"u",
        "core-metadata":{"sha256":"abc"},"dist-info-metadata":{"sha256":"abc"}}]}"#;
    let parsed = crate::pypi::parse_detail(json.as_bytes()).unwrap();
    assert_eq!(
        (&parsed.files[0].core_metadata, &parsed.files[0].dist_info_metadata),
        (
            &CoreMetadata::Hashes(sha256("abc")),
            &CoreMetadata::Hashes(sha256("abc"))
        )
    );
}

#[test]
fn test_parse_detail_reads_legacy_only_metadata_key() {
    let json = r#"{"name":"x","files":[{"filename":"x-1.whl","url":"u","dist-info-metadata":true}]}"#;
    let parsed = crate::pypi::parse_detail(json.as_bytes()).unwrap();
    assert_eq!(
        (&parsed.files[0].core_metadata, &parsed.files[0].dist_info_metadata),
        (&CoreMetadata::Absent, &CoreMetadata::Available)
    );
    assert_eq!(parsed.files[0].metadata(), &CoreMetadata::Available);
}

#[test]
fn test_file_metadata_helpers_update_both_spellings() {
    let mut file = File {
        filename: "x-1.whl".to_owned(),
        url: "u".to_owned(),
        hashes: BTreeMap::new(),
        requires_python: None,
        size: None,
        upload_time: None,
        yanked: Yanked::No,
        core_metadata: CoreMetadata::Absent,
        dist_info_metadata: CoreMetadata::Available,
        gpg_sig: None,
        provenance: Provenance::Absent,
    };
    assert_eq!(file.metadata(), &CoreMetadata::Available);
    file.set_metadata(CoreMetadata::Hashes(sha256("abc")));
    assert_eq!(
        (&file.core_metadata, &file.dist_info_metadata),
        (
            &CoreMetadata::Hashes(sha256("abc")),
            &CoreMetadata::Hashes(sha256("abc"))
        )
    );
    file.clear_metadata();
    assert_eq!(
        (&file.core_metadata, &file.dist_info_metadata),
        (&CoreMetadata::Absent, &CoreMetadata::Absent)
    );
}

#[test]
fn test_parse_detail_reads_project_status_provenance_gpg_size_upload_time_and_versions() {
    let json = r#"{"meta":{"api-version":"1.4","project-status":"archived",
        "project-status-reason":"read only"},"name":"x","versions":["1.0"],
        "files":[{"filename":"x-1.whl","url":"u","hashes":{},"size":42,
        "upload-time":"2024-01-01T00:00:00Z","gpg-sig":false,
        "provenance":"https://example.test/x-1.whl.provenance"}]}"#;
    let parsed = crate::pypi::parse_detail(json.as_bytes()).unwrap();
    assert_eq!(
        (
            parsed.meta.project_status.as_deref(),
            parsed.meta.project_status_reason.as_deref(),
            parsed.versions.as_slice(),
            parsed.files[0].size,
            parsed.files[0].upload_time.as_deref(),
            parsed.files[0].gpg_sig,
            &parsed.files[0].provenance,
        ),
        (
            Some("archived"),
            Some("read only"),
            ["1.0".to_owned()].as_slice(),
            Some(42),
            Some("2024-01-01T00:00:00Z"),
            Some(false),
            &Provenance::Url("https://example.test/x-1.whl.provenance".to_owned()),
        )
    );
}

#[test]
fn test_parse_detail_rejects_unsupported_major_api_version() {
    let err = crate::pypi::parse_detail(br#"{"meta":{"api-version":"2.0"},"name":"x"}"#).unwrap_err();
    assert!(matches!(err, SimpleError::UnsupportedApiVersion(version) if version == "2.0"));
}

#[test]
fn test_parse_detail_rejects_invalid_api_version() {
    for version in ["1", "x.0", "1.x"] {
        let page = format!(r#"{{"meta":{{"api-version":"{version}"}},"name":"x"}}"#);
        let err = crate::pypi::parse_detail(page.as_bytes()).unwrap_err();
        assert!(matches!(&err, SimpleError::InvalidApiVersion(invalid) if invalid == version));
        assert_eq!(
            err.to_string(),
            format!("invalid upstream Simple API version {version:?}; expected Major.Minor")
        );
        assert!(err.source().is_none());
    }
}

#[test]
fn test_parse_meta_reads_project_status() {
    let meta = crate::pypi::parse_meta(
        br#"{"api-version":"1.4","project-status":"archived","project-status-reason":"read only"}"#,
    )
    .unwrap();
    assert_eq!(meta.project_status.as_deref(), Some("archived"));
    assert_eq!(meta.project_status_reason.as_deref(), Some("read only"));
    assert_eq!(meta.status(), ProjectStatus::Archived);
    assert!(!meta.status().allows_uploads());
    assert!(meta.status().offers_downloads());
}

#[test]
fn test_parse_meta_rejects_invalid_project_status() {
    let err = crate::pypi::parse_meta(br#"{"api-version":"1.4","project-status":"frozen"}"#).unwrap_err();
    assert!(matches!(&err, SimpleError::InvalidProjectStatus(status) if status == "frozen"));
    assert_eq!(err.to_string(), "invalid upstream project status marker \"frozen\"");
    assert!(err.source().is_none());
}

#[test]
fn test_project_status_policy() {
    assert_eq!(ProjectStatus::Active.marker(), "active");
    assert_eq!(ProjectStatus::Archived.marker(), "archived");
    assert_eq!(ProjectStatus::Quarantined.marker(), "quarantined");
    assert_eq!(ProjectStatus::Deprecated.marker(), "deprecated");
    assert!(ProjectStatus::Active.allows_uploads());
    assert!(ProjectStatus::Deprecated.allows_uploads());
    assert!(!ProjectStatus::Archived.allows_uploads());
    assert!(!ProjectStatus::Quarantined.allows_uploads());
    assert!(!ProjectStatus::Quarantined.offers_downloads());
}

#[test]
fn test_simple_error_json_source() {
    let err = crate::pypi::parse_detail(b"not json").unwrap_err();
    assert!(matches!(err, SimpleError::Json(_)));
    assert!(err.source().is_some());
    assert!(err.to_string().contains("expected"));
}

#[test]
fn test_simple_error_html_source() {
    let err = SimpleError::from(tl::ParseError::InvalidLength);
    assert!(matches!(err, SimpleError::Html(tl::ParseError::InvalidLength)));
    assert!(err.source().is_some());
    assert_eq!(
        err.to_string(),
        "invalid upstream Simple API HTML: The input string length is too large to fit in a `u32`"
    );
}

#[test]
fn test_yanked_deserialize_variants() {
    assert_eq!(serde_json::from_str::<Yanked>("false").unwrap(), Yanked::No);
    assert_eq!(serde_json::from_str::<Yanked>("true").unwrap(), Yanked::Yes);
    assert_eq!(
        serde_json::from_str::<Yanked>("\"why\"").unwrap(),
        Yanked::Reason("why".to_owned())
    );
}

#[test]
fn test_yanked_deserialize_rejects_number() {
    assert!(serde_json::from_str::<Yanked>("123").is_err());
}

#[test]
fn test_core_metadata_deserialize_variants() {
    assert_eq!(
        serde_json::from_str::<CoreMetadata>("false").unwrap(),
        CoreMetadata::Absent
    );
    assert_eq!(
        serde_json::from_str::<CoreMetadata>("true").unwrap(),
        CoreMetadata::Available
    );
    let hashes = serde_json::from_str::<CoreMetadata>(r#"{"sha256":"abc"}"#).unwrap();
    assert_eq!(hashes, CoreMetadata::Hashes(sha256("abc")));
}

#[test]
fn test_core_metadata_deserialize_rejects_number() {
    assert!(serde_json::from_str::<CoreMetadata>("123").is_err());
}

#[test]
fn test_provenance_deserialize_variants() {
    assert_eq!(serde_json::from_str::<Provenance>("null").unwrap(), Provenance::None);
    assert_eq!(
        serde_json::from_str::<Provenance>(r#""https://example.test/provenance""#).unwrap(),
        Provenance::Url("https://example.test/provenance".to_owned())
    );
    assert!(serde_json::from_str::<Provenance>("123").is_err());
}
