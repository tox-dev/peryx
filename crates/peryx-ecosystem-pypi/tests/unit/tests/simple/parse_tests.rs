use std::collections::BTreeMap;

use super::{sample_detail, sample_list, sha256};
use crate::{
    CoreMetadata, File, Meta, ProjectDetail, ProjectList, ProjectListEntry, Provenance, Yanked, parse_index,
    render_legacy_json, to_json,
};

#[test]
fn test_detail_json_snapshot() {
    insta::assert_snapshot!("detail_json", to_json(&sample_detail()));
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
fn test_parse_detail_roundtrips_serialized_model() {
    let detail = sample_detail();
    let parsed = crate::parse_detail(to_json(&detail).as_bytes()).unwrap();
    assert_eq!(parsed.meta, detail.meta);
    assert_eq!(parsed.name, detail.name);
    assert_eq!(parsed.versions, detail.versions);
    assert_eq!(parsed.files, detail.files);
}

#[test]
fn test_parse_detail_minimal() {
    let parsed = crate::parse_detail(b"{\"name\":\"x\"}").unwrap();
    // A page that advertises no version promises no PEP 700 fields, so it maps to the base version.
    assert_eq!(
        parsed.meta,
        Meta {
            api_version: crate::API_VERSION_BASE,
            ..Meta::default()
        }
    );
    assert_eq!(parsed.name, "x");
    assert!(parsed.versions.is_empty());
    assert!(parsed.files.is_empty());
}

#[test]
fn test_parse_detail_reads_both_metadata_spellings() {
    let json = r#"{"name":"x","files":[{"filename":"x-1.whl","url":"u",
        "core-metadata":{"sha256":"abc"},"dist-info-metadata":{"sha256":"abc"}}]}"#;
    let parsed = crate::parse_detail(json.as_bytes()).unwrap();
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
    let parsed = crate::parse_detail(json.as_bytes()).unwrap();
    assert_eq!(
        (&parsed.files[0].core_metadata, &parsed.files[0].dist_info_metadata),
        (&CoreMetadata::Absent, &CoreMetadata::Available)
    );
    assert_eq!(parsed.files[0].metadata(), &CoreMetadata::Available);
}

#[test]
fn test_parse_detail_reads_project_status_provenance_gpg_size_upload_time_and_versions() {
    let json = r#"{"meta":{"api-version":"1.4","project-status":"archived",
        "project-status-reason":"read only"},"name":"x","versions":["1.0"],
        "files":[{"filename":"x-1.whl","url":"u","hashes":{},"size":42,
        "upload-time":"2024-01-01T00:00:00Z","gpg-sig":false,
        "provenance":"https://example.test/x-1.whl.provenance"}]}"#;
    let parsed = crate::parse_detail(json.as_bytes()).unwrap();
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
fn test_legacy_project_json_maps_simple_fields() {
    let detail = sample_detail();
    let legacy: serde_json::Value = serde_json::from_str(&render_legacy_json(&detail, None, None).unwrap()).unwrap();

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
fn test_legacy_project_json_carries_resolved_contacts() {
    let metadata = crate::CoreMetadataDoc {
        author: Some("Jane".to_owned()),
        author_email: Some("jane@example.test".to_owned()),
        maintainer: Some("Joe".to_owned()),
        maintainer_email: Some("joe@example.test".to_owned()),
        ..crate::CoreMetadataDoc::default()
    };
    let legacy: serde_json::Value =
        serde_json::from_str(&render_legacy_json(&sample_detail(), None, Some(&metadata)).unwrap()).unwrap();

    let info = &legacy["info"];
    assert_eq!(info["author"], "Jane");
    assert_eq!(info["author_email"], "jane@example.test");
    assert_eq!(info["maintainer"], "Joe");
    assert_eq!(info["maintainer_email"], "joe@example.test");
}

#[test]
fn test_legacy_release_json_omits_releases_and_matches_equivalent_version() {
    let detail = sample_detail();
    let legacy: serde_json::Value =
        serde_json::from_str(&render_legacy_json(&detail, Some("1.0.0"), None).unwrap()).unwrap();

    assert_eq!(legacy.get("releases"), None);
    assert_eq!(legacy["info"]["version"], "1.0");
    assert_eq!(legacy["info"]["yanked"], true);
    assert_eq!(legacy["urls"][0]["filename"], "proj-1.0-py3-none-any.whl");
    assert_eq!(legacy["urls"][0]["yanked_reason"], serde_json::Value::Null);
}

#[test]
fn test_legacy_release_json_rejects_unknown_version() {
    assert_eq!(render_legacy_json(&sample_detail(), Some("9.9"), None), None);
}

#[test]
fn test_legacy_release_json_resolves_filename_only_version() {
    let mut detail = sample_detail();
    detail.versions = vec!["not-a-version".to_owned()];

    let legacy: serde_json::Value =
        serde_json::from_str(&render_legacy_json(&detail, Some("1.5"), None).unwrap()).unwrap();

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
    let legacy: serde_json::Value = serde_json::from_str(&render_legacy_json(&detail, None, None).unwrap()).unwrap();

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
fn test_legacy_project_json_groups_unparseable_version_and_dashless_file() {
    let detail = ProjectDetail {
        meta: Meta::default(),
        name: "proj".to_owned(),
        versions: vec!["not-a-version".to_owned(), "1.0".to_owned()],
        files: vec![
            File {
                filename: "proj-1.0-py3-none-any.whl".to_owned(),
                url: "https://files.example/a.whl".to_owned(),
                hashes: BTreeMap::new(),
                requires_python: None,
                size: None,
                upload_time: None,
                yanked: Yanked::No,
                core_metadata: CoreMetadata::Absent,
                dist_info_metadata: CoreMetadata::Absent,
                gpg_sig: None,
                provenance: Provenance::Absent,
            },
            File {
                filename: "dashless".to_owned(),
                url: "https://files.example/x".to_owned(),
                hashes: BTreeMap::new(),
                requires_python: None,
                size: None,
                upload_time: None,
                yanked: Yanked::No,
                core_metadata: CoreMetadata::Absent,
                dist_info_metadata: CoreMetadata::Absent,
                gpg_sig: None,
                provenance: Provenance::Absent,
            },
        ],
    };
    let legacy: serde_json::Value = serde_json::from_str(&render_legacy_json(&detail, None, None).unwrap()).unwrap();

    assert_eq!(legacy["releases"]["1.0"][0]["filename"], "proj-1.0-py3-none-any.whl");
    assert_eq!(legacy["releases"]["not-a-version"], serde_json::json!([]));
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
    let legacy: serde_json::Value = serde_json::from_str(&render_legacy_json(&detail, None, None).unwrap()).unwrap();

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
    let legacy: serde_json::Value = serde_json::from_str(&render_legacy_json(&detail, None, None).unwrap()).unwrap();

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

#[derive(Default)]
struct Collect(Vec<File>);

impl crate::simple::DetailSink for Collect {
    type Error = std::convert::Infallible;

    fn file(&mut self, file: File) -> Result<(), Self::Error> {
        self.0.push(file);
        Ok(())
    }
}

struct Boom;

impl crate::simple::DetailSink for Boom {
    type Error = String;

    fn file(&mut self, _file: File) -> Result<(), Self::Error> {
        Err("sink rejected the file".to_owned())
    }
}

fn detail_base() -> url::Url {
    url::Url::parse("https://pypi.org/simple/flask/").unwrap()
}

#[test]
fn test_stream_detail_json_collects_files_and_absolutizes_urls() {
    let body = br#"{"meta":{"api-version":"1.1"},"name":"flask","versions":["1.0"],
        "files":[{"filename":"flask-1.0.tar.gz","url":"../../files/flask-1.0.tar.gz","hashes":{"sha256":"abc"}}],
        "alternate-locations":["ignored"]}"#;
    let mut sink = Collect::default();

    let detail: crate::simple::StreamedDetail =
        crate::simple::stream_detail_json(std::io::Cursor::new(&body[..]), &detail_base(), &mut sink).unwrap();

    assert_eq!(detail.name, "flask");
    assert_eq!(detail.versions, vec!["1.0".to_owned()]);
    assert_eq!(sink.0.len(), 1);
    assert_eq!(sink.0[0].url, "https://pypi.org/files/flask-1.0.tar.gz");
}

#[test]
fn test_stream_detail_json_defaults_a_missing_name() {
    let body = br#"{"meta":{"api-version":"1.0"},"files":[]}"#;
    let mut sink = Collect::default();
    let detail = crate::simple::stream_detail_json(std::io::Cursor::new(&body[..]), &detail_base(), &mut sink).unwrap();
    assert_eq!(detail.name, "");
    assert!(sink.0.is_empty());
}

#[test]
fn test_stream_detail_json_rejects_unsupported_api_version() {
    let body = br#"{"meta":{"api-version":"2.0"},"files":[]}"#;
    let mut sink = Collect::default();
    assert!(crate::simple::stream_detail_json(std::io::Cursor::new(&body[..]), &detail_base(), &mut sink).is_err());
}

#[test]
fn test_stream_detail_json_rejects_invalid_json() {
    let mut sink = Collect::default();
    assert!(crate::simple::stream_detail_json(std::io::Cursor::new(&b"{"[..]), &detail_base(), &mut sink).is_err());
}

#[test]
fn test_stream_detail_json_rejects_trailing_data() {
    let body = br#"{"meta":{"api-version":"1.0"},"files":[]} trailing"#;
    let mut sink = Collect::default();
    assert!(crate::simple::stream_detail_json(std::io::Cursor::new(&body[..]), &detail_base(), &mut sink).is_err());
}

#[test]
fn test_stream_detail_json_rejects_a_non_object_root() {
    let mut sink = Collect::default();
    assert!(crate::simple::stream_detail_json(std::io::Cursor::new(&b"[]"[..]), &detail_base(), &mut sink).is_err());
}

#[test]
fn test_stream_detail_json_rejects_non_array_files() {
    let mut sink = Collect::default();
    assert!(
        crate::simple::stream_detail_json(std::io::Cursor::new(&br#"{"files":{}}"#[..]), &detail_base(), &mut sink)
            .is_err()
    );
}

#[test]
fn test_stream_detail_json_surfaces_a_sink_error() {
    let body = br#"{"files":[{"filename":"f","url":"u","hashes":{}}]}"#;
    let mut sink = Boom;
    assert!(crate::simple::stream_detail_json(std::io::Cursor::new(&body[..]), &detail_base(), &mut sink).is_err());
}
