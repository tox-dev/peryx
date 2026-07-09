use std::collections::BTreeMap;

use super::{sample_detail, sample_list};
use crate::{CoreMetadata, File, Meta, ProjectDetail, Provenance, Yanked, render_detail_html, render_index_html};

#[test]
fn test_detail_html_snapshot() {
    insta::assert_snapshot!("detail_html", render_detail_html(&sample_detail()));
}

#[test]
fn test_index_html_snapshot() {
    insta::assert_snapshot!("index_html", render_index_html(&sample_list()));
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
