use std::collections::BTreeMap;

use crate::pypi::{
    CoreMetadata, File, Meta, ProjectDetail, ProjectList, ProjectListEntry, Yanked, render_detail_html,
    render_index_html, to_json,
};

fn sha256(value: &str) -> BTreeMap<String, String> {
    BTreeMap::from([("sha256".to_owned(), value.to_owned())])
}

/// A detail whose three files together exercise every field and enum variant, plus HTML escaping
/// of `&`, `<`, `>` (text) and `&`, `<`, `>`, `"` (attributes).
fn sample_detail() -> ProjectDetail {
    ProjectDetail {
        meta: Meta::default(),
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
    assert_eq!(parsed.name, detail.name);
    assert_eq!(parsed.versions, detail.versions);
    assert_eq!(parsed.files, detail.files);
}

#[test]
fn test_parse_detail_minimal() {
    let parsed = crate::pypi::parse_detail(b"{\"name\":\"x\"}").unwrap();
    assert_eq!(parsed.name, "x");
    assert!(parsed.versions.is_empty());
    assert!(parsed.files.is_empty());
}

#[test]
fn test_parse_detail_accepts_legacy_metadata_key() {
    let json = r#"{"name":"x","files":[{"filename":"x-1.whl","url":"u","dist-info-metadata":true}]}"#;
    let parsed = crate::pypi::parse_detail(json.as_bytes()).unwrap();
    assert_eq!(parsed.files[0].core_metadata, CoreMetadata::Available);
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
