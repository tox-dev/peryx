use std::collections::BTreeMap;

use super::{sample_detail, sample_list};
use crate::{
    CoreMetadata, File, Meta, ProjectDetail, ProjectList, ProjectListEntry, Provenance, Yanked, render_detail_html,
    render_index_html,
};

#[test]
fn test_detail_html_snapshot() {
    insta::assert_snapshot!("detail_html", render_detail_html(&sample_detail()));
}

#[test]
fn test_index_html_snapshot() {
    insta::assert_snapshot!("index_html", render_index_html(&sample_list()));
}

#[test]
fn test_render_detail_html_non_sha256_metadata_hash_advertises_true() {
    let hashes = BTreeMap::from([("sha512".to_owned(), "abc".to_owned())]);
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

    assert!(html.contains("data-core-metadata=\"true\""));
    assert!(html.contains("data-dist-info-metadata=\"true\""));
}

#[test]
fn test_render_detail_html_falls_back_to_non_sha256_hash_fragment() {
    let html = render_detail_html(&ProjectDetail {
        meta: Meta::default(),
        name: "proj".to_owned(),
        versions: vec!["1.0".to_owned()],
        files: vec![File {
            filename: "proj-1.0.tar.gz".to_owned(),
            url: "https://files.example/proj-1.0.tar.gz".to_owned(),
            hashes: BTreeMap::from([("md5".to_owned(), "deadbeef".to_owned())]),
            requires_python: None,
            size: None,
            upload_time: None,
            yanked: Yanked::No,
            core_metadata: CoreMetadata::Absent,
            dist_info_metadata: CoreMetadata::Absent,
            gpg_sig: None,
            provenance: Provenance::Absent,
        }],
    });

    assert!(html.contains("#md5=deadbeef"));
}

#[test]
fn test_render_detail_html_escapes_hash_fragment() {
    let html = render_detail_html(&ProjectDetail {
        meta: Meta::default(),
        name: "proj".to_owned(),
        versions: vec!["1.0".to_owned()],
        files: vec![File {
            filename: "proj-1.0.tar.gz".to_owned(),
            url: "https://files.example/proj-1.0.tar.gz".to_owned(),
            hashes: BTreeMap::from([("sha256".to_owned(), "de\" onclick=alert(1) x=\"".to_owned())]),
            requires_python: None,
            size: None,
            upload_time: None,
            yanked: Yanked::No,
            core_metadata: CoreMetadata::Absent,
            dist_info_metadata: CoreMetadata::Absent,
            gpg_sig: None,
            provenance: Provenance::Absent,
        }],
    });

    assert!(html.contains("#sha256=de&quot; onclick=alert(1) x=&quot;"));
    assert!(!html.contains("de\" onclick=alert(1)"));
}

#[test]
fn test_render_detail_html_escapes_core_metadata_hash() {
    let hashes = BTreeMap::from([("sha256".to_owned(), "ab\" onload=alert(1) x=\"".to_owned())]);
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

    assert!(html.contains("data-core-metadata=\"sha256=ab&quot; onload=alert(1) x=&quot;\""));
    assert!(html.contains("data-dist-info-metadata=\"sha256=ab&quot; onload=alert(1) x=&quot;\""));
    assert!(!html.contains("onload=alert(1) x=\"\""));
}

#[test]
fn test_render_index_html_percent_encodes_route_injection_in_href() {
    let list = index_of(&["x\" onmouseover=\"alert(1)"]);

    let html = render_index_html(&list);

    assert!(html.contains("<a href=\"x%22%20onmouseover%3D%22alert%281%29/\">"));
}

#[test]
fn test_render_index_html_normalized_route_survives_encoding_unchanged() {
    let list = index_of(&["Flask"]);

    let html = render_index_html(&list);

    assert!(html.contains("<a href=\"flask/\">Flask</a>"));
}

#[test]
fn test_render_index_html_injects_no_attributes_for_hostile_names() {
    let names = ["x\" onmouseover=\"alert(1)", "a&b", "<script>", "café-π"];
    let list = index_of(&names);

    let html = render_index_html(&list);

    let dom = tl::parse(&html, tl::ParserOptions::default()).expect("valid HTML");
    let anchors: Vec<_> = dom
        .nodes()
        .iter()
        .filter_map(|node| node.as_tag())
        .filter(|tag| tag.name().as_bytes().eq_ignore_ascii_case(b"a"))
        .collect();
    assert_eq!(anchors.len(), names.len());
    for anchor in anchors {
        let attrs: Vec<_> = anchor.attributes().iter().collect();
        assert_eq!(attrs.len(), 1, "only href, got {attrs:?}");
        assert!(attrs[0].0.eq_ignore_ascii_case("href"));
    }
}

fn index_of(names: &[&str]) -> ProjectList {
    ProjectList {
        meta: Meta::default(),
        projects: names
            .iter()
            .map(|name| ProjectListEntry {
                name: (*name).to_owned(),
            })
            .collect(),
    }
}
