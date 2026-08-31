use std::collections::BTreeMap;

use peryx_storage::meta::{ArtifactOrigin as _, ArtifactSource, ByteAvailability};

use super::{
    FilePublication, FileSource, MetaStore, MetadataClaim, PypiArtifactOrigin, metadata_key, split_file_source,
};
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
        split_file_source("source", "https://files.example/pkg.whl\npypi\n\nmirror").unwrap(),
        FileSource {
            url: "https://files.example/pkg.whl".to_owned(),
            source: "pypi".to_owned(),
            size: None,
            upstream: Some("mirror".to_owned()),
        }
    );
}

#[test]
fn test_put_and_get_metadata_roundtrips_the_derived_digest() {
    let (_dir, meta) = store();
    assert_eq!(meta.get_metadata_digest("wheelsha").unwrap(), None);
    meta.put_metadata("wheelsha", "metasha").unwrap();
    assert_eq!(
        meta.get_metadata_digest("wheelsha").unwrap(),
        Some("metasha".to_owned())
    );
}

#[test]
fn test_get_metadata_digest_skips_a_non_utf8_record() {
    let (_dir, meta) = store();
    meta.put_driver_value(&metadata_key("bad"), &[0xff, 0xfe]).unwrap();

    assert_eq!(meta.get_metadata_digest("bad").unwrap(), None);
}

#[test]
fn test_get_metadata_digests_skips_missing_records() {
    let (_dir, meta) = store();
    meta.put_metadata("wheelsha", "metasha").unwrap();

    let digests = meta.get_metadata_digests(["missing", "wheelsha"]).unwrap();

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
    meta.put_metadata("wheelsha", "metasha").unwrap();
    let mut seen = Vec::new();
    meta.scan_metadata_records(|digest, value| {
        seen.push((digest.to_owned(), value.to_owned()));
        Ok::<(), std::io::Error>(())
    })
    .unwrap();
    assert_eq!(seen, vec![("wheelsha".to_owned(), "metasha".to_owned())]);
}

#[test]
fn test_scan_metadata_records_skips_a_non_utf8_record() {
    let (_dir, meta) = store();
    meta.put_metadata("good", "metasha").unwrap();
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

fn seed_publication(meta: &MetaStore, value: &[u8]) {
    meta.put_driver_value(&super::publication_key("pypi", "pkg", "wheelsha", "pkg-1.0.whl"), value)
        .unwrap();
}

#[test]
fn test_get_file_publication_reads_a_claim_with_its_routed_upstream() {
    let (_dir, meta) = store();
    seed_publication(&meta, b"https://up/pkg.whl.metadata\nmetasha\npypi\nmirror");

    assert_eq!(
        meta.get_file_publication("pypi", "pkg", "wheelsha", "pkg-1.0.whl")
            .unwrap(),
        Some(FilePublication::Claimed(MetadataClaim {
            url: "https://up/pkg.whl.metadata".to_owned(),
            metadata_sha256: "metasha".to_owned(),
            source: "pypi".to_owned(),
            upstream: Some("mirror".to_owned()),
        }))
    );
}

#[test]
fn test_get_file_publication_reads_an_empty_record_as_unclaimed() {
    let (_dir, meta) = store();
    seed_publication(&meta, b"");

    assert_eq!(
        meta.get_file_publication("pypi", "pkg", "wheelsha", "pkg-1.0.whl")
            .unwrap(),
        Some(FilePublication::Unclaimed)
    );
}

#[test]
fn test_get_file_publication_is_absent_for_an_unpublished_file() {
    let (_dir, meta) = store();

    assert_eq!(
        meta.get_file_publication("pypi", "pkg", "wheelsha", "pkg-1.0.whl")
            .unwrap(),
        None
    );
}

#[rstest::rstest]
#[case::without_digest(b"https://up/pkg.whl.metadata", "metadata_sha256")]
#[case::without_source(b"https://up/pkg.whl.metadata\nmetasha", "source")]
#[case::without_upstream(b"https://up/pkg.whl.metadata\nmetasha\npypi", "upstream")]
fn test_get_file_publication_rejects_a_truncated_claim(#[case] value: &[u8], #[case] field: &str) {
    let (_dir, meta) = store();
    seed_publication(&meta, value);

    let err = meta
        .get_file_publication("pypi", "pkg", "wheelsha", "pkg-1.0.whl")
        .unwrap_err();

    assert!(
        matches!(err, peryx_storage::meta::MetaError::DriverRecordMissing { field: missing, .. } if missing == field)
    );
}

#[test]
fn test_get_file_publication_rejects_a_non_utf8_record() {
    let (_dir, meta) = store();
    seed_publication(&meta, &[0xff, 0xfe]);

    assert!(matches!(
        meta.get_file_publication("pypi", "pkg", "wheelsha", "pkg-1.0.whl")
            .unwrap_err(),
        peryx_storage::meta::MetaError::DriverRecordUtf8 { .. }
    ));
}

#[test]
fn test_scan_file_publications_visits_valid_and_skips_non_utf8() {
    let (_dir, meta) = store();
    seed_publication(&meta, b"https://up/pkg.whl.metadata\nmetasha\npypi\n");
    meta.put_driver_value(&super::publication_key("pypi", "pkg", "other", "pkg-2.0.whl"), &[0xff])
        .unwrap();
    let mut seen = Vec::new();

    meta.scan_file_publications(|key, value| {
        seen.push((key.to_owned(), value.to_owned()));
        Ok::<(), std::io::Error>(())
    })
    .unwrap();

    assert_eq!(
        seen,
        vec![(
            "pypi/pkg/wheelsha/pkg-1.0.whl".to_owned(),
            "https://up/pkg.whl.metadata\nmetasha\npypi\n".to_owned()
        )]
    );
}
