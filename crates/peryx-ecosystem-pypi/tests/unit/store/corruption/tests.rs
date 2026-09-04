use std::convert::Infallible;

use peryx_storage::meta::{MetaError, MetaScanError, MetaStore, RepairScan};
use rstest::rstest;

use super::{
    FileOverride, OverrideMutation, ProvenanceSibling, PypiRecords, PypiStore as _, file_key, metadata_key,
    override_key, project_key, provenance_key, publication_key,
};

const HEALTHY: &str = "aa";
const CORRUPT: &str = "bb";
const NOT_UTF8: [u8; 2] = [0xff, 0xfe];

fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, meta)
}

/// One well-formed row in every namespace this issue names, so each case proves the healthy row is
/// still read while the damaged one is reported.
fn seed_healthy(meta: &MetaStore) {
    meta.put_file_url("pypi", "pkg", HEALTHY, "https://files.example/aa.whl", "pypi")
        .unwrap();
    meta.put_metadata(HEALTHY, "metasha").unwrap();
    meta.put_driver_value(&publication_key("pypi", "pkg", HEALTHY, "pkg-1.0.whl"), b"")
        .unwrap();
    meta.put_project("pypi", HEALTHY, "Aa").unwrap();
    meta.set_override(false, "hosted", "pkg", "aa-1.0.whl", OverrideMutation::Hidden(true), 1)
        .unwrap();
    meta.put_provenance(
        "hosted",
        "pkg",
        HEALTHY,
        "pkg-1.0.whl",
        ProvenanceSibling {
            provenance_sha256: "provsha",
            size: 16,
        },
    )
    .unwrap();
}

fn corrupt_key(namespace: PypiRecords) -> String {
    match namespace {
        PypiRecords::FileUrl => file_key("pypi", "pkg", CORRUPT),
        PypiRecords::Metadata => metadata_key(CORRUPT),
        PypiRecords::Publication => publication_key("pypi", "pkg", CORRUPT, "pkg-2.0.whl"),
        PypiRecords::Project => project_key("pypi", CORRUPT),
        PypiRecords::Override => override_key("hosted", "pkg", "bb-2.0.whl"),
        PypiRecords::Provenance => provenance_key("hosted", "pkg", CORRUPT, "pkg-2.0.whl"),
    }
}

/// The healthy row's key as a scan reports it: relative to the namespace prefix.
fn healthy_row(namespace: PypiRecords) -> &'static str {
    match namespace {
        PypiRecords::FileUrl => "pypi/pkg/aa",
        PypiRecords::Metadata => "aa",
        PypiRecords::Publication => "pypi/pkg/aa/pkg-1.0.whl",
        PypiRecords::Project => "pypi/aa",
        PypiRecords::Override => "hosted/pkg/aa-1.0.whl",
        PypiRecords::Provenance => "hosted/pkg/aa/pkg-1.0.whl",
    }
}

fn corrupt_row(namespace: PypiRecords) -> &'static str {
    match namespace {
        PypiRecords::FileUrl => "pypi/pkg/bb",
        PypiRecords::Metadata => "bb",
        PypiRecords::Publication => "pypi/pkg/bb/pkg-2.0.whl",
        PypiRecords::Project => "pypi/bb",
        PypiRecords::Override => "hosted/pkg/bb-2.0.whl",
        PypiRecords::Provenance => "hosted/pkg/bb/pkg-2.0.whl",
    }
}

fn scan(meta: &MetaStore, namespace: PypiRecords) -> Result<Vec<String>, MetaScanError<Infallible>> {
    let mut seen = Vec::new();
    {
        let mut visit = |key: &str, _value: &str| {
            seen.push(key.to_owned());
            Ok::<(), Infallible>(())
        };
        match namespace {
            PypiRecords::FileUrl => meta.scan_file_urls(|index, normalized, digest, value| {
                visit(&format!("{index}/{normalized}/{digest}"), value)
            }),
            PypiRecords::Metadata => meta.scan_metadata_records(&mut visit),
            PypiRecords::Publication => meta.scan_file_publications(&mut visit),
            PypiRecords::Project => meta.scan_project_records(&mut visit),
            PypiRecords::Override => meta.scan_override_records(&mut visit),
            PypiRecords::Provenance => meta.scan_provenance_records(&mut visit),
        }?;
    }
    Ok(seen)
}

fn repair_scan(meta: &MetaStore, namespace: PypiRecords) -> (RepairScan, Vec<String>) {
    let mut seen = Vec::new();
    let scan = meta
        .scan_records_for_repair(namespace, |key, _value| {
            seen.push(key.to_owned());
            Ok::<(), Infallible>(())
        })
        .unwrap();
    (scan, seen)
}

fn corrupt_keys(scan: &RepairScan) -> Vec<String> {
    scan.corrupt().iter().map(|record| record.key.clone()).collect()
}

fn not_utf8_message(key: &str) -> String {
    format!("driver record {key:?} is not UTF-8")
}

#[rstest]
#[case::file_url(PypiRecords::FileUrl)]
#[case::metadata(PypiRecords::Metadata)]
#[case::publication(PypiRecords::Publication)]
#[case::project(PypiRecords::Project)]
#[case::overrides(PypiRecords::Override)]
#[case::provenance(PypiRecords::Provenance)]
fn test_a_normal_scan_visits_every_intact_record(#[case] namespace: PypiRecords) {
    let (_dir, meta) = store();
    seed_healthy(&meta);

    assert_eq!(scan(&meta, namespace).unwrap(), vec![healthy_row(namespace).to_owned()]);
}

#[rstest]
#[case::file_url(PypiRecords::FileUrl)]
#[case::metadata(PypiRecords::Metadata)]
#[case::publication(PypiRecords::Publication)]
#[case::project(PypiRecords::Project)]
#[case::overrides(PypiRecords::Override)]
#[case::provenance(PypiRecords::Provenance)]
fn test_a_normal_scan_stops_at_a_record_that_is_not_utf8(#[case] namespace: PypiRecords) {
    let (_dir, meta) = store();
    seed_healthy(&meta);
    let key = corrupt_key(namespace);
    meta.put_driver_value(&key, &NOT_UTF8).unwrap();

    assert_eq!(scan(&meta, namespace).unwrap_err().to_string(), not_utf8_message(&key));
}

#[rstest]
#[case::file_url(PypiRecords::FileUrl)]
#[case::metadata(PypiRecords::Metadata)]
#[case::publication(PypiRecords::Publication)]
#[case::project(PypiRecords::Project)]
#[case::overrides(PypiRecords::Override)]
#[case::provenance(PypiRecords::Provenance)]
fn test_a_repair_scan_over_intact_records_reports_itself_complete(#[case] namespace: PypiRecords) {
    let (_dir, meta) = store();
    seed_healthy(&meta);

    let (scan, seen) = repair_scan(&meta, namespace);

    assert_eq!(
        (scan.is_incomplete(), corrupt_keys(&scan), seen),
        (false, Vec::new(), vec![healthy_row(namespace).to_owned()])
    );
}

#[rstest]
#[case::file_url(PypiRecords::FileUrl)]
#[case::metadata(PypiRecords::Metadata)]
#[case::publication(PypiRecords::Publication)]
#[case::project(PypiRecords::Project)]
#[case::overrides(PypiRecords::Override)]
#[case::provenance(PypiRecords::Provenance)]
fn test_a_repair_scan_names_the_record_it_could_not_read(#[case] namespace: PypiRecords) {
    let (_dir, meta) = store();
    seed_healthy(&meta);
    meta.put_driver_value(&corrupt_key(namespace), &NOT_UTF8).unwrap();

    let (scan, seen) = repair_scan(&meta, namespace);

    assert_eq!(
        (scan.is_incomplete(), corrupt_keys(&scan), seen),
        (
            true,
            vec![corrupt_row(namespace).to_owned()],
            vec![healthy_row(namespace).to_owned()]
        )
    );
}

fn read_file_url(meta: &MetaStore) -> Result<(), MetaError> {
    meta.get_file_url("pypi", "pkg", CORRUPT).map(drop)
}

fn read_metadata_digest(meta: &MetaStore) -> Result<(), MetaError> {
    meta.get_metadata_digest(CORRUPT).map(drop)
}

fn read_file_publication(meta: &MetaStore) -> Result<(), MetaError> {
    meta.get_file_publication("pypi", "pkg", CORRUPT, "pkg-2.0.whl")
        .map(drop)
}

fn read_project(meta: &MetaStore) -> Result<(), MetaError> {
    meta.get_project("pypi", CORRUPT).map(drop)
}

fn read_projects(meta: &MetaStore) -> Result<(), MetaError> {
    meta.list_projects("pypi").map(drop)
}

fn read_overrides(meta: &MetaStore) -> Result<(), MetaError> {
    meta.list_overrides("hosted", "pkg").map(drop)
}

fn read_provenance(meta: &MetaStore) -> Result<(), MetaError> {
    meta.get_provenance("hosted", "pkg", CORRUPT, "pkg-2.0.whl").map(drop)
}

fn hide_the_corrupt_override(meta: &MetaStore) -> Result<(), MetaError> {
    meta.set_override(false, "hosted", "pkg", "bb-2.0.whl", OverrideMutation::Hidden(true), 2)
        .map(drop)
}

type PointRead = fn(&MetaStore) -> Result<(), MetaError>;

#[rstest]
#[case::file_url(PypiRecords::FileUrl, read_file_url)]
#[case::metadata(PypiRecords::Metadata, read_metadata_digest)]
#[case::publication(PypiRecords::Publication, read_file_publication)]
#[case::project(PypiRecords::Project, read_project)]
#[case::project_listing(PypiRecords::Project, read_projects)]
#[case::overrides(PypiRecords::Override, read_overrides)]
#[case::override_mutation(PypiRecords::Override, hide_the_corrupt_override)]
#[case::provenance(PypiRecords::Provenance, read_provenance)]
fn test_a_point_read_reports_a_record_that_is_not_utf8(#[case] namespace: PypiRecords, #[case] read: PointRead) {
    let (_dir, meta) = store();
    seed_healthy(&meta);
    let key = corrupt_key(namespace);
    meta.put_driver_value(&key, &NOT_UTF8).unwrap();

    assert_eq!(read(&meta).unwrap_err().to_string(), not_utf8_message(&key));
}

#[rstest]
#[case::file_url(read_file_url)]
#[case::metadata(read_metadata_digest)]
#[case::publication(read_file_publication)]
#[case::project(read_project)]
#[case::project_listing(read_projects)]
#[case::overrides(read_overrides)]
#[case::provenance(read_provenance)]
fn test_a_point_read_of_an_absent_record_is_not_an_error(#[case] read: PointRead) {
    let (_dir, meta) = store();
    seed_healthy(&meta);

    assert!(read(&meta).is_ok());
}

#[test]
fn test_a_mutation_over_a_malformed_override_does_not_replace_it_with_a_default() {
    let (_dir, meta) = store();
    let key = override_key("hosted", "pkg", "bb-2.0.whl");
    meta.put_driver_value(&key, b"{}").unwrap();

    let error = hide_the_corrupt_override(&meta).unwrap_err();

    assert_eq!(
        (error.to_string(), meta.get_driver_value(&key).unwrap()),
        (format!("driver record {key:?} does not decode"), Some(b"{}".to_vec()))
    );
}

#[test]
fn test_an_override_from_a_newer_peryx_reads_as_a_schema_error_not_damage() {
    let (_dir, meta) = store();
    let key = override_key("hosted", "pkg", "bb-2.0.whl");
    meta.put_driver_value(&key, br#"{"hidden":true,"yanked":false,"quarantined":true}"#)
        .unwrap();

    let error = meta.list_overrides("hosted", "pkg").unwrap_err();

    assert_eq!(
        error.to_string(),
        format!("driver record {key:?} carries unknown field \"quarantined\" and needs a newer peryx")
    );
}

#[test]
fn test_an_intact_override_still_reads_as_the_record_it_was_written_as() {
    let (_dir, meta) = store();
    seed_healthy(&meta);

    assert_eq!(
        meta.list_overrides("hosted", "pkg").unwrap(),
        std::collections::BTreeMap::from([(
            "aa-1.0.whl".to_owned(),
            FileOverride {
                hidden: true,
                yanked: crate::Yanked::No,
            }
        )])
    );
}
