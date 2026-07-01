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
