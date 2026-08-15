use std::collections::BTreeMap;

use peryx_storage::meta::{ArtifactOrigin as _, ArtifactSource, ByteAvailability};

use super::{FileSource, MetaStore, PypiArtifactOrigin, metadata_key, split_file_source};
use crate::store::PypiStore as _;

fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, meta)
}

#[test]
fn test_put_and_get_file_url() {
    let (_dir, meta) = store();
    assert_eq!(meta.get_file_url("deadbeef").unwrap(), None);
    meta.put_file_url("deadbeef", "https://files.example/pkg.whl", "pypi")
        .unwrap();
    assert_eq!(
        meta.get_file_url("deadbeef").unwrap(),
        Some(FileSource {
            url: "https://files.example/pkg.whl".to_owned(),
            source: "pypi".to_owned(),
            size: None,
            upstream: None,
        })
    );
}

#[test]
fn test_origin_maps_to_the_neutral_source() {
    assert_eq!(PypiArtifactOrigin::Upload.artifact_source(), ArtifactSource::Hosted);
    assert_eq!(PypiArtifactOrigin::Cached.artifact_source(), ArtifactSource::Proxy);
}

#[test]
fn test_recording_a_cached_locator_projects_a_remote_only_placement() {
    let (_dir, meta) = store();
    meta.initialize_distributed_state().unwrap();
    meta.put_file_url("deadbeef", "https://files.example/pkg.whl", "pypi")
        .unwrap();
    assert_eq!(
        meta.get_artifact_placement("deadbeef").unwrap().unwrap().availability,
        ByteAvailability::RemoteOnly
    );
}

#[test]
fn test_file_source_without_size_keeps_routed_upstream() {
    assert_eq!(
        split_file_source("https://files.example/pkg.whl\npypi\n\nmirror"),
        Some(FileSource {
            url: "https://files.example/pkg.whl".to_owned(),
            source: "pypi".to_owned(),
            size: None,
            upstream: Some("mirror".to_owned()),
        })
    );
}

#[test]
fn test_put_and_get_metadata_roundtrips_the_sibling() {
    let (_dir, meta) = store();
    assert_eq!(meta.get_metadata("wheelsha").unwrap(), None);
    meta.put_metadata("wheelsha", "https://up/pkg.whl.metadata", "metasha", "pypi")
        .unwrap();
    assert_eq!(
        meta.get_metadata("wheelsha").unwrap(),
        Some((
            "https://up/pkg.whl.metadata".to_owned(),
            "metasha".to_owned(),
            "pypi".to_owned(),
        ))
    );
}

#[test]
fn test_get_metadata_digests_skips_missing_and_malformed_records() {
    let (_dir, meta) = store();
    meta.put_metadata("wheelsha", "https://up/pkg.whl.metadata", "metasha", "pypi")
        .unwrap();
    // Legacy records may lack the digest separator.
    meta.put_driver_value(&metadata_key("broken"), b"only-url").unwrap();

    let digests = meta.get_metadata_digests(["missing", "broken", "wheelsha"]).unwrap();

    assert_eq!(digests, BTreeMap::from([("wheelsha".to_owned(), "metasha".to_owned())]));
}

#[test]
fn test_scan_file_urls_visits_each_record() {
    let (_dir, meta) = store();
    meta.put_file_url("aa", "https://files/aa.whl", "pypi").unwrap();
    let mut seen = Vec::new();
    meta.scan_file_urls(|digest, value| {
        seen.push((digest.to_owned(), value.to_owned()));
        Ok::<(), std::io::Error>(())
    })
    .unwrap();
    assert_eq!(seen, vec![("aa".to_owned(), "https://files/aa.whl\npypi".to_owned())]);
}

#[test]
fn test_scan_file_urls_skips_a_non_utf8_record() {
    let (_dir, meta) = store();
    meta.put_file_url("aa", "https://files/aa.whl", "pypi").unwrap();
    meta.put_driver_value(&super::file_key("bad"), &[0xff, 0xfe]).unwrap();
    let mut count = 0;
    meta.scan_file_urls(|_digest, _value| {
        count += 1;
        Ok::<(), std::io::Error>(())
    })
    .unwrap();
    assert_eq!(count, 1, "the non-UTF-8 record is skipped, the valid one visited");
}

#[test]
fn test_scan_metadata_records_visits_each_record() {
    let (_dir, meta) = store();
    meta.put_metadata("wheelsha", "https://up/pkg.metadata", "metasha", "pypi")
        .unwrap();
    let mut seen = Vec::new();
    meta.scan_metadata_records(|digest, value| {
        seen.push((digest.to_owned(), value.to_owned()));
        Ok::<(), std::io::Error>(())
    })
    .unwrap();
    assert_eq!(
        seen,
        vec![(
            "wheelsha".to_owned(),
            "https://up/pkg.metadata\nmetasha\npypi".to_owned()
        )]
    );
}

#[test]
fn test_scan_metadata_records_skips_a_non_utf8_record() {
    let (_dir, meta) = store();
    meta.put_metadata("good", "https://up/pkg.metadata", "metasha", "pypi")
        .unwrap();
    meta.put_driver_value(&metadata_key("bad"), &[0xff, 0xfe]).unwrap();
    let mut seen = Vec::new();
    meta.scan_metadata_records(|digest, _value| {
        seen.push(digest.to_owned());
        Ok::<(), std::io::Error>(())
    })
    .unwrap();
    assert_eq!(seen, vec!["good".to_owned()], "the non-UTF-8 record is skipped");
}

#[test]
fn test_put_and_get_provenance_roundtrips_the_sibling() {
    let (_dir, meta) = store();
    assert_eq!(meta.get_provenance("wheelsha").unwrap(), None);
    meta.put_provenance("wheelsha", "provsha", 16).unwrap();
    assert_eq!(
        meta.get_provenance("wheelsha").unwrap(),
        Some(("provsha".to_owned(), 16))
    );
}

#[test]
fn test_get_provenance_rejects_a_record_missing_its_size() {
    let (_dir, meta) = store();
    meta.put_driver_value(&super::provenance_key("wheelsha"), b"provsha")
        .unwrap();
    assert_eq!(meta.get_provenance("wheelsha").unwrap(), None);
}

#[test]
fn test_scan_provenance_records_visits_valid_and_skips_non_utf8() {
    let (_dir, meta) = store();
    meta.put_provenance("good", "provsha", 16).unwrap();
    meta.put_driver_value(&super::provenance_key("bad"), &[0xff, 0xfe])
        .unwrap();
    let mut seen = Vec::new();
    meta.scan_provenance_records(|digest, value| {
        seen.push((digest.to_owned(), value.to_owned()));
        Ok::<(), std::io::Error>(())
    })
    .unwrap();
    assert_eq!(seen, vec![("good".to_owned(), "provsha\n16".to_owned())]);
}
