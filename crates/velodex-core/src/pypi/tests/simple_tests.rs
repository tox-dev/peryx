use std::collections::BTreeMap;
use std::error::Error as _;

use crate::pypi::{
    CoreMetadata, File, Meta, ProjectDetail, ProjectList, ProjectListEntry, Provenance, SimpleError, Yanked,
    render_detail_html, render_index_html, to_json,
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
fn test_index_html_snapshot() {
    insta::assert_snapshot!("index_html", render_index_html(&sample_list()));
}

#[test]
fn test_index_json_snapshot() {
    insta::assert_snapshot!("index_json", to_json(&sample_list()));
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
