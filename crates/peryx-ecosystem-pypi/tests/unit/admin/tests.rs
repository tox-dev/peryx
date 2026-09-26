use peryx_identity::IndexAcl;
use std::convert::Infallible;

use peryx_index::{Index, IndexKind};
use peryx_policy::{Policy, PolicyConfig};
use peryx_storage::blob::{BlobStorage, BlobStore, Digest};
use peryx_storage::meta::{DriverMutation, MetaError, MetaScanError, MetaStore};
use rstest::rstest;

use super::*;
use crate::store::CachedIndex;
use crate::upload::Uploaded;
use crate::{CoreMetadata, File, Provenance, Yanked};

const DIGEST_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const DIGEST_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, meta)
}

fn seed_valid_page(meta: &MetaStore) {
    let digest = Digest::of(b"wheel");
    let metadata_digest = Digest::of(b"metadata");
    let body = format!(
        r#"{{"meta":{{"api-version":"1.1"}},"name":"flask","versions":["1.0"],"files":[{{"filename":"flask-1.0.whl","size":11,"url":"https://files/flask.whl","hashes":{{"sha256":"{d}"}},"core-metadata":{{"sha256":"{m}"}},"yanked":false}}]}}"#,
        d = digest.as_str(),
        m = metadata_digest.as_str(),
    );
    meta.put_index(
        "pypi/flask",
        &CachedIndex {
            source: None,
            last_modified: None,
            etag: None,
            last_serial: None,
            fetched_at_unix: 0,
            content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
            fresh_secs: Some(1),
            body: body.into_bytes(),
        },
    )
    .unwrap();
    meta.put_project("pypi", "flask", "Flask").unwrap();
    meta.put_file_url("pypi", "flask", digest.as_str(), "https://files/flask.whl", "pypi")
        .unwrap();
    meta.put_metadata(digest.as_str(), metadata_digest.as_str()).unwrap();
    meta.put_driver_value(
        &format!(
            "pypi\u{0}n\u{0}pypi/flask/{digest}/flask-1.0.whl",
            digest = digest.as_str()
        ),
        format!("https://files/flask.whl.metadata\n{}\npypi\n", metadata_digest.as_str()).as_bytes(),
    )
    .unwrap();
}

fn upload_record(filename: &str, digest: &str) -> Uploaded {
    Uploaded {
        version: "1.0".to_owned(),
        file: File {
            filename: filename.to_owned(),
            url: "u".to_owned(),
            hashes: std::collections::BTreeMap::from([("sha256".to_owned(), digest.to_owned())]),
            requires_python: None,
            size: None,
            upload_time: None,
            yanked: Yanked::No,
            core_metadata: CoreMetadata::Absent,
            dist_info_metadata: CoreMetadata::Absent,
            gpg_sig: None,
            provenance: Provenance::Absent,
            authoritative_version: None,
        },
        imports: None,
        trashed: None,
    }
}

fn upload_value(digest: Option<&str>) -> Vec<u8> {
    let mut upload = upload_record("flask.whl", DIGEST_A);
    if let Some(digest) = digest {
        upload.file.hashes.insert("sha256".to_owned(), digest.to_owned());
    } else {
        upload.file.hashes.clear();
    }
    serde_json::to_vec(&upload).unwrap()
}

#[test]
fn test_error_message_renders_store_and_visit_scan_faults() {
    let decode = serde_json::from_str::<u8>("x").unwrap_err();
    assert!(!crate::error_message(MetaScanError::<Infallible>::from(MetaError::Decode(decode))).is_empty());
    assert_eq!(crate::error_message(MetaScanError::Visit("boom".to_owned())), "boom");
    assert_eq!(
        crate::error_message(MetaScanError::Visit(std::io::Error::other("disk"))).as_str(),
        "disk"
    );
}

#[test]
fn test_referenced_blob_digests_keeps_a_claimed_sidecar_without_a_derived_record() {
    let (_dir, meta) = store();
    let sidecar = Digest::of(b"claimed sidecar");
    meta.put_driver_value(
        "pypi\u{0}n\u{0}pypi/flask/wheelsha/flask-1.0.whl",
        format!("https://files/flask.whl.metadata\n{}\npypi\n", sidecar.as_str()).as_bytes(),
    )
    .unwrap();
    meta.put_driver_value("pypi\u{0}n\u{0}pypi/flask/othersha/flask-2.0.whl", b"")
        .unwrap();

    assert_eq!(
        referenced_blob_digests(&meta).unwrap(),
        std::collections::BTreeSet::from([sidecar.as_str().to_owned()])
    );
}

#[test]
fn test_referenced_blob_digests_rejects_a_corrupt_publication_record() {
    let (_dir, meta) = store();
    meta.put_driver_value(
        "pypi\u{0}n\u{0}pypi/flask/wheelsha/flask-1.0.whl",
        b"url\nnot-hex\npypi",
    )
    .unwrap();

    assert!(referenced_blob_digests(&meta).is_err());
}

#[test]
fn test_cache_pages_lists_the_stored_pages_split_by_index() {
    let (_dir, meta) = store();
    seed_valid_page(&meta);
    let pages = cache_pages(&meta, &["pypi"]).unwrap();
    assert_eq!(pages.len(), 1);
    assert_eq!((pages[0].index.as_str(), pages[0].resource.as_str()), ("pypi", "flask"));
}

#[test]
fn test_cache_pages_splits_an_unconfigured_index_key() {
    let (_dir, meta) = store();
    seed_valid_page(&meta);
    meta.put_index("root", &meta.get_index("pypi/flask").unwrap().unwrap())
        .unwrap();

    let pages = cache_pages(&meta, &[])
        .unwrap()
        .into_iter()
        .map(|page| (page.index, page.resource))
        .collect::<std::collections::BTreeSet<_>>();

    assert_eq!(
        pages,
        std::collections::BTreeSet::from([
            ("pypi".to_owned(), "flask".to_owned()),
            ("root".to_owned(), String::new()),
        ])
    );
}

#[test]
fn test_cache_record_counts_counts_each_record_kind() {
    let (_dir, meta) = store();
    seed_valid_page(&meta);
    meta.put_upload("pypi", "flask", "flask-1.0.whl", br#"{"version":"1.0"}"#)
        .unwrap();
    meta.set_override(
        true,
        "pypi",
        "flask",
        "flask-1.0.whl",
        crate::store::OverrideMutation::Yanked(&Yanked::Yes),
        0,
    )
    .unwrap();
    meta.put_provenance(
        "pypi",
        "flask",
        &"a".repeat(64),
        "flask-1.0.whl",
        provenance_bundle(&"b".repeat(64)),
    )
    .unwrap();
    let counts: std::collections::HashMap<String, u64> = cache_record_counts(&meta).unwrap().into_iter().collect();
    assert_eq!(counts["file_url_records"], 1);
    assert_eq!(counts["metadata_records"], 1);
    assert_eq!(counts["publication_records"], 1);
    assert_eq!(counts["project_records"], 1);
    assert_eq!(counts["upload_records"], 1);
    assert_eq!(counts["override_records"], 1);
    assert_eq!(counts["provenance_records"], 1);
}

#[test]
fn test_referenced_blob_digests_rejects_a_corrupt_file_url_record() {
    let (_dir, meta) = store();

    meta.put_driver_value("pypi\u{0}f\u{0}pypi/flask/not-hex", b"https://files/x\npypi")
        .unwrap();
    assert!(referenced_blob_digests(&meta).is_err());
}

#[test]
fn test_checkpoint_blob_digests_reads_the_folded_rows() {
    let mut state = CheckpointState::default();
    state
        .apply(
            vec![
                DriverMutation::Put {
                    key: format!("pypi\0f\0pypi/flask/{DIGEST_A}"),
                    value: b"https://files/flask.whl\npypi".to_vec(),
                },
                DriverMutation::Put {
                    key: format!("pypi\0d\0{DIGEST_A}"),
                    value: DIGEST_B.as_bytes().to_vec(),
                },
                DriverMutation::Put {
                    key: format!("pypi\0n\0pypi/flask/{DIGEST_A}/flask-1.0.whl"),
                    value: format!("https://files/flask.whl.metadata\n{DIGEST_B}\npypi\n").into_bytes(),
                },
                DriverMutation::Put {
                    key: "pypi\0n\0pypi/flask/unclaimed/flask-1.0.whl".to_owned(),
                    value: Vec::new(),
                },
                DriverMutation::Put {
                    key: "pypi\0u\0pypi/flask/flask-1.0.whl".to_owned(),
                    value: crate::to_json(&upload_record("flask-1.0.whl", DIGEST_A)).into_bytes(),
                },
                DriverMutation::Put {
                    key: format!("pypi\0a\0pypi/flask/{DIGEST_A}/flask-1.0.whl"),
                    value: format!("{DIGEST_B}\n16").into_bytes(),
                },
                DriverMutation::Put {
                    key: "unrelated".to_owned(),
                    value: b"ignored".to_vec(),
                },
            ],
            Vec::new(),
            b"{}",
        )
        .unwrap();

    assert_eq!(
        checkpoint_blob_digests(&state).unwrap(),
        std::collections::BTreeSet::from([DIGEST_A.to_owned(), DIGEST_B.to_owned()])
    );
}

#[test]
fn test_checkpoint_blob_digests_reads_a_provenance_row() {
    let mut state = CheckpointState::default();
    state
        .apply(
            vec![DriverMutation::Put {
                key: format!("pypi\0a\0pypi/flask/{DIGEST_A}/flask-1.0.whl"),
                value: format!("{DIGEST_B}\n16").into_bytes(),
            }],
            Vec::new(),
            b"{}",
        )
        .unwrap();

    assert_eq!(
        checkpoint_blob_digests(&state).unwrap(),
        std::collections::BTreeSet::from([DIGEST_B.to_owned()])
    );
}

#[test]
fn test_checkpoint_blob_digests_rejects_a_corrupt_folded_row() {
    for (key, value) in [
        ("pypi\0f\0invalid".to_owned(), b"invalid".to_vec()),
        (
            "pypi\0f\0pypi/flask/not-hex".to_owned(),
            b"https://files/flask.whl\npypi".to_vec(),
        ),
        (format!("pypi\0d\0{DIGEST_A}"), b"not-hex".to_vec()),
        (
            format!("pypi\0n\0pypi/flask/{DIGEST_A}/flask-1.0.whl"),
            b"invalid".to_vec(),
        ),
        ("pypi\0u\0pypi/flask/flask-1.0.whl".to_owned(), b"invalid".to_vec()),
        (
            format!("pypi\0a\0pypi/flask/{DIGEST_A}/flask-1.0.whl"),
            b"invalid".to_vec(),
        ),
    ] {
        let mut state = CheckpointState::default();
        state
            .apply(vec![DriverMutation::Put { key, value }], Vec::new(), b"{}")
            .unwrap();

        assert!(checkpoint_blob_digests(&state).is_err());
    }
}

#[test]
fn test_referenced_blob_digests_rejects_a_corrupt_metadata_record() {
    let (_dir, meta) = store();

    meta.put_driver_value("pypi\u{0}d\u{0}not-hex", b"https://files/x.metadata\nabc\npypi")
        .unwrap();
    assert!(referenced_blob_digests(&meta).is_err());
}

#[test]
fn test_referenced_blob_digests_rejects_each_metadata_digest_fault() {
    for (key, value) in [
        ("a".repeat(64), "missing-fields".to_owned()),
        ("a".repeat(64), "url\nnot-hex\npypi".to_owned()),
    ] {
        let (_dir, meta) = store();
        meta.put_driver_value(&format!("pypi\u{0}d\u{0}{key}"), value.as_bytes())
            .unwrap();
        assert!(referenced_blob_digests(&meta).is_err());
    }
}

#[test]
fn test_referenced_blob_digests_rejects_a_corrupt_upload_record() {
    let (_dir, meta) = store();
    meta.put_upload("pypi", "flask", "flask-1.0.whl", b"not json").unwrap();
    assert!(referenced_blob_digests(&meta).is_err());
}

fn provenance_bundle(provenance_sha256: &str) -> crate::store::ProvenanceSibling<'_> {
    crate::store::ProvenanceSibling {
        provenance_sha256,
        size: 16,
    }
}

#[test]
fn test_referenced_blob_digests_includes_the_provenance_blob() {
    let (_dir, meta) = store();
    let provenance_blob = "c".repeat(64);
    meta.put_provenance(
        "pypi",
        "flask",
        DIGEST_A,
        "flask-1.0.whl",
        provenance_bundle(&provenance_blob),
    )
    .unwrap();
    assert!(referenced_blob_digests(&meta).unwrap().contains(&provenance_blob));
}

#[rstest]
#[case::without_size(DIGEST_B.to_owned())]
#[case::provenance("not-hex\n16".to_owned())]
#[case::size(format!("{DIGEST_B}\ninvalid"))]
fn test_referenced_blob_digests_rejects_each_corrupt_provenance_field(#[case] value: String) {
    let (_dir, meta) = store();

    meta.put_driver_value(
        &format!("pypi\u{0}a\u{0}pypi/flask/{DIGEST_A}/flask-1.0.whl"),
        value.as_bytes(),
    )
    .unwrap();

    assert!(referenced_blob_digests(&meta).is_err());
}

#[test]
fn test_fsck_metadata_reports_every_invalid_record_kind() {
    let (dir, meta) = store();
    let blobs: BlobStorage = BlobStore::new(dir.path().join("blobs")).into();
    meta.put_driver_value("pypi\u{0}i\u{0}pypi/flask", b"garbage").unwrap();
    meta.put_driver_value("pypi\u{0}f\u{0}not-hex", b"u\npypi").unwrap();
    meta.put_driver_value("pypi\u{0}d\u{0}not-hex", b"u\nm\npypi").unwrap();
    meta.put_driver_value("pypi\u{0}p\u{0}pypi/flask", b"").unwrap();
    meta.put_upload("pypi", "flask", "flask-1.0.whl", b"not json").unwrap();
    meta.put_driver_value("pypi\u{0}o\u{0}pypi/flask/flask-1.0.whl", b"bogus")
        .unwrap();
    meta.put_driver_value("pypi\u{0}a\u{0}pypi/flask/not-hex/flask-1.0.whl", b"abc\n16")
        .unwrap();

    meta.put_provenance("pypi", "flask", DIGEST_A, "flask-1.0.whl", provenance_bundle(DIGEST_B))
        .unwrap();
    let mut out = Vec::new();
    let problems = fsck_metadata(&meta, &blobs, &audited_fixture(), &mut out).unwrap();
    // Seven damaged records, plus the count row: the project above is written straight into the store
    // while the upload goes through the write path, so the index's count row is short by one project.
    assert_eq!(problems, 8, "{}", String::from_utf8_lossy(&out));
}

#[rstest]
#[case::pep658_artifact('d', "not-hex", format!("url\n{DIGEST_B}\npypi"), "pep658", 1)]
#[case::pep658_metadata('d', DIGEST_A, "url\nnot-hex\npypi".to_owned(), "pep658", 1)]
#[case::file_url_digest('f', "pypi/flask/not-hex", "u\npypi".to_owned(), "file-url", 1)]
#[case::project_index('p', "/flask", "Flask".to_owned(), "project", 1)]
#[case::project_name('p', "pypi/", "Flask".to_owned(), "project", 1)]
#[case::project_display('p', "pypi/flask", String::new(), "project", 2)]
#[case::publication_metadata('n', "pypi/demo/sha/demo-1.0.whl", "url\nnot-hex\npypi\n".to_owned(), "publication", 1)]
#[case::publication_truncated('n', "pypi/demo/sha/demo-1.0.whl", "url".to_owned(), "publication", 1)]
#[case::override_filename('o', "hosted/demo/", r#"{"hidden":true,"yanked":false}"#.to_owned(), "override", 1)]
#[case::override_kind('o', "hosted/demo/demo.whl", "invalid".to_owned(), "override", 1)]
fn test_fsck_metadata_rejects_each_invalid_field(
    #[case] table: char,
    #[case] key: &str,
    #[case] value: String,
    #[case] record: &str,
    // `project_display` is the one row here that is well enough keyed to be counted. Writing it
    // straight into the store leaves its index without the count row a real write would have
    // maintained, which the summary audit reports in its own right.
    #[case] problems: u64,
) {
    let (dir, meta) = store();
    let blobs = BlobStore::new(dir.path().join("blobs")).into();
    meta.put_driver_value(&format!("pypi\u{0}{table}\u{0}{key}"), value.as_bytes())
        .unwrap();
    let mut output = Vec::new();

    assert_eq!(
        fsck_metadata(&meta, &blobs, &audited_fixture(), &mut output).unwrap(),
        problems
    );
    assert!(
        String::from_utf8(output)
            .unwrap()
            .starts_with(&format!("metadata\tpypi\t{record}\t{key}\t"))
    );
}

#[test]
fn test_fsck_metadata_rejects_an_invalid_upload_key_with_present_blobs() {
    let (dir, meta) = store();
    let blobs: BlobStorage = BlobStore::new(dir.path().join("blobs")).into();
    let digest = blobs.blocking().put_bytes(b"artifact").unwrap();
    let uploaded = upload_record("demo.whl", digest.as_str());
    meta.put_driver_value("pypi\u{0}u\u{0}hosted/demo/", crate::to_json(&uploaded).as_bytes())
        .unwrap();
    let mut output = Vec::new();

    assert_eq!(
        fsck_metadata(&meta, &blobs, &audited_fixture(), &mut output).unwrap(),
        1
    );
    assert_eq!(
        String::from_utf8(output).unwrap(),
        "metadata\tpypi\tupload\thosted/demo/\tinvalid key\n"
    );
}

#[test]
fn test_policy_dry_run_reports_a_corrupt_cached_page() {
    let (_dir, meta) = store();
    meta.put_driver_value("pypi\u{0}i\u{0}pypi/flask", b"garbage").unwrap();
    let indexes = [pypi_index()];
    let mut out = Vec::new();
    assert!(policy_dry_run(&meta, &indexes, None, None, &mut out).is_err());
}

#[test]
fn test_policy_dry_run_reports_a_corrupt_upload_record() {
    let (_dir, meta) = store();
    meta.put_upload("pypi", "flask", "flask-1.0.whl", b"not json").unwrap();
    let indexes = [pypi_index()];
    let mut out = Vec::new();
    assert!(policy_dry_run(&meta, &indexes, None, None, &mut out).is_err());
}

fn seed_undecodable_detail(meta: &MetaStore, key: &str) {
    meta.put_index(
        key,
        &CachedIndex {
            source: None,
            last_modified: None,
            etag: None,
            last_serial: None,
            fetched_at_unix: 0,
            content_type: None,
            fresh_secs: None,
            body: b"not a project detail document".to_vec(),
        },
    )
    .unwrap();
}

#[test]
fn test_policy_dry_run_reports_a_page_whose_body_is_not_a_detail() {
    let (_dir, meta) = store();
    seed_undecodable_detail(&meta, "pypi/flask");
    let indexes = [pypi_index()];
    let mut out = Vec::new();
    assert!(policy_dry_run(&meta, &indexes, None, None, &mut out).is_err());
}

#[test]
fn test_purge_project_counts_the_removed_records() {
    let (_dir, meta) = store();
    seed_valid_page(&meta);
    let digest = Digest::of(b"preserved upload");
    let uploaded = upload_record("other-1.0.tar.gz", digest.as_str());
    meta.put_upload(
        "hosted",
        "other",
        "other-1.0.tar.gz",
        crate::to_json(&uploaded).as_bytes(),
    )
    .unwrap();
    let report = super::purge_project(&meta, "pypi", "flask", false).unwrap();
    assert_eq!(report.resource, "flask");
    let index_pages = report
        .categories
        .iter()
        .find(|(label, _)| label == "index_pages")
        .map(|(_, count)| *count);
    assert_eq!(index_pages, Some(1));
}

#[test]
fn test_purge_project_reports_a_corrupt_preserved_page() {
    let (_dir, meta) = store();
    seed_valid_page(&meta);

    seed_undecodable_detail(&meta, "pypi/other");
    assert!(super::purge_project(&meta, "pypi", "flask", false).is_err());
}

#[test]
fn test_purge_project_reports_a_target_page_that_is_not_a_detail() {
    let (_dir, meta) = store();
    seed_undecodable_detail(&meta, "pypi/flask");

    let error = purge_project(&meta, "pypi", "flask", false).unwrap_err();

    assert!(error.contains("read cached project pypi/flask"), "{error}");
}

#[test]
fn test_purge_project_scopes_a_corrupt_target_record() {
    let (_dir, meta) = store();
    meta.put_driver_value("pypi\u{0}i\u{0}pypi/flask", b"not json").unwrap();

    let error = purge_project(&meta, "pypi", "flask", false).unwrap_err();

    assert!(error.contains("read cached project pypi/flask"), "{error}");
    assert!(error.contains("expected ident at line 1 column 2"), "{error}");
}

#[test]
fn test_purge_project_handles_missing_and_applied_targets() {
    let (_dir, meta) = store();
    assert_eq!(
        purge_project(&meta, "pypi", "missing", false).unwrap().resource,
        "missing"
    );
    seed_valid_page(&meta);
    let report = purge_project(&meta, "pypi", "Flask", true).unwrap();
    assert_eq!(report.resource, "flask");
    assert!(meta.get_index("pypi/flask").unwrap().is_none());
}

#[test]
fn test_purge_project_rejects_a_corrupt_preserved_upload() {
    let (_dir, meta) = store();
    meta.put_upload("hosted", "demo", "demo.whl", b"bad").unwrap();
    assert!(purge_project(&meta, "pypi", "flask", false).is_err());
}

#[test]
fn test_fsck_reports_invalid_upload_keys_and_missing_blobs() {
    let (dir, meta) = store();
    let blobs = BlobStore::new(dir.path().join("blobs")).into();
    let digest = Digest::of(b"missing");
    let uploaded = upload_record("demo.whl", digest.as_str());
    meta.put_driver_value("pypi\u{0}u\u{0}bad", crate::to_json(&uploaded).as_bytes())
        .unwrap();
    meta.put_upload("hosted", "demo", "demo.whl", crate::to_json(&uploaded).as_bytes())
        .unwrap();
    let mut output = Vec::new();
    assert_eq!(
        fsck_metadata(&meta, &blobs, &audited_fixture(), &mut output).unwrap(),
        2
    );
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("invalid key"));
    assert!(output.contains("missing blob"));
}

/// The indexes the metadata checks run against: every fixture writes under one of these two names.
fn audited_fixture() -> Vec<Index> {
    vec![pypi_index(), hosted_index()]
}

fn cached_index() -> Index {
    cached_index_with_name("cached")
}

fn cached_index_with_name(name: &str) -> Index {
    Index {
        name: name.to_owned(),
        route: name.to_owned(),
        kind: IndexKind::Cached {
            client: peryx_upstream::UpstreamClient::new("https://example.invalid/simple/").unwrap(),
            offline: true,
        },
        ..pypi_index()
    }
}

fn cached_pypi_index() -> Index {
    Index {
        name: "pypi".to_owned(),
        route: "pypi".to_owned(),
        kind: IndexKind::Cached {
            client: peryx_upstream::UpstreamClient::new("https://example.invalid/simple/").unwrap(),
            offline: true,
        },
        ..pypi_index()
    }
}

fn virtual_index() -> Index {
    Index {
        name: "layered".to_owned(),
        route: "layered".to_owned(),
        kind: IndexKind::Virtual {
            layers: Vec::new(),
            write_target: None,
        },
        ..pypi_index()
    }
}

/// A cached index owns rows and is audited against them; a virtual index owns none, so a derived row
/// naming one is a row that should not exist rather than a count that disagrees.
#[test]
fn test_fsck_audits_a_cached_index_and_disowns_a_virtual_one() {
    let (dir, meta) = store();
    let blobs = BlobStore::new(dir.path().join("blobs")).into();
    meta.put_driver_value("pypi\u{0}p\u{0}cached/flask", b"Flask").unwrap();
    meta.put_driver_value("pypi\u{0}k\u{0}layered", b"1\n1").unwrap();
    let mut output = Vec::new();

    let problems = fsck_metadata(&meta, &blobs, &[cached_index(), virtual_index()], &mut output).unwrap();

    assert_eq!(
        (problems, String::from_utf8(output).unwrap()),
        (
            2,
            format!(
                "metadata\tpypi\tsummary-count\t{:?}\tno cached or hosted index owns this row\nmetadata\tpypi\tsummary-count\t{:?}\tcount row is absent, rows hold 1 projects and 0 uploads\n",
                "pypi\u{0}k\u{0}layered", "pypi\u{0}k\u{0}cached"
            )
        )
    );
}

/// An index belonging to another ecosystem names no `PyPI` row, so it is not audited here.
#[test]
fn test_fsck_ignores_an_index_from_another_ecosystem() {
    let (dir, meta) = store();
    let blobs = BlobStore::new(dir.path().join("blobs")).into();
    meta.put_driver_value("pypi\u{0}k\u{0}hosted", b"1\n1").unwrap();
    let mut output = Vec::new();

    let problems = fsck_metadata(&meta, &blobs, &[oci_hosted_index()], &mut output).unwrap();

    assert_eq!(
        (problems, String::from_utf8(output).unwrap()),
        (
            1,
            format!(
                "metadata\tpypi\tsummary-count\t{:?}\tno cached or hosted index owns this row\n",
                "pypi\u{0}k\u{0}hosted"
            )
        )
    );
}

/// Only a `PyPI` index that owns rows delimits a source key, so a slash inside the name of any other
/// index is read as the key's own separator and leaves no digest in its last segment.
#[rstest]
#[case::another_ecosystem(oci_hosted_index())]
#[case::virtual_index(virtual_index())]
fn test_fsck_splits_a_source_key_only_on_an_index_owning_rows(#[case] index: Index) {
    let (dir, meta) = store();
    let blobs = BlobStore::new(dir.path().join("blobs")).into();
    let key = format!("org/team/flask/{DIGEST_A}");
    meta.put_driver_value(&format!("pypi\u{0}f\u{0}{key}"), b"u\norg/team")
        .unwrap();
    let mut output = Vec::new();

    let problems = fsck_metadata(&meta, &blobs, &[named_org_team(index)], &mut output).unwrap();

    assert_eq!(
        (problems, String::from_utf8(output).unwrap()),
        (1, format!("metadata\tpypi\tfile-url\t{key}\tinvalid record\n"))
    );
}

fn oci_hosted_index() -> Index {
    Index {
        ecosystem: peryx_core::Ecosystem::new("oci"),
        ..hosted_index()
    }
}

/// A name holding a slash, so a key under it splits differently once the index is known.
fn named_org_team(index: Index) -> Index {
    Index {
        name: "org/team".to_owned(),
        ..index
    }
}

fn pypi_index() -> Index {
    Index {
        name: "pypi".to_owned(),
        route: "pypi".to_owned(),
        ecosystem: crate::ECOSYSTEM,
        kind: IndexKind::Hosted { volatile: false },
        policy: Policy::default(),
        acl: IndexAcl::default(),
    }
}

fn hosted_index() -> Index {
    Index {
        name: "hosted".to_owned(),
        route: "hosted".to_owned(),
        ecosystem: crate::ECOSYSTEM,
        kind: IndexKind::Hosted { volatile: false },
        policy: Policy::default(),
        acl: IndexAcl::default(),
    }
}

fn blocked_index() -> Index {
    Index {
        route: "root/pypi".to_owned(),
        policy: Policy::compile(
            &PolicyConfig {
                block_resources: vec!["flask".to_owned()],
                ..PolicyConfig::default()
            },
            crate::normalize_name,
        ),
        ..pypi_index()
    }
}

#[rstest]
#[case::index_name(Some("pypi"), None, true)]
#[case::index_route(Some("root/pypi"), None, true)]
#[case::project(None, Some("Flask"), true)]
#[case::other_index(Some("other"), None, false)]
#[case::other_project(None, Some("other"), false)]
fn test_policy_dry_run_filters_by_index_name_route_and_project(
    #[case] index: Option<&str>,
    #[case] project: Option<&str>,
    #[case] denied: bool,
) {
    let (_dir, meta) = store();
    seed_valid_page(&meta);
    let mut output = Vec::new();

    policy_dry_run(&meta, &[blocked_index()], index, project, &mut output).unwrap();

    assert_eq!(!output.is_empty(), denied);
}

#[test]
fn test_policy_dry_run_skips_uploads_it_cannot_attribute() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();

    meta.put_upload("ghost", "proj", "file.whl", br#"{"version":"1.0"}"#)
        .unwrap();

    meta.put_upload("hosted", "flask", "flask-1.0.whl", br#"{"version":"1.0"}"#)
        .unwrap();

    meta.put_driver_value("pypi\u{0}u\u{0}noslashkey", b"x").unwrap();

    let indexes = [hosted_index()];
    let mut out = Vec::new();
    policy_dry_run(&meta, &indexes, None, Some("other"), &mut out).unwrap();

    assert_eq!(String::from_utf8(out).unwrap(), "");
}

#[test]
fn test_policy_dry_run_filters_cached_pages() {
    let (_dir, meta) = store();
    seed_valid_page(&meta);
    let indexes = [pypi_index()];
    for (index, project) in [(Some("other"), None), (None, Some("other"))] {
        let mut output = Vec::new();
        policy_dry_run(&meta, &indexes, index, project, &mut output).unwrap();
        assert!(output.is_empty());
    }
}

#[test]
fn test_policy_dry_run_reports_upload_denials() {
    let (_dir, meta) = store();
    let uploaded = upload_record("demo-1.0-py3-none-any.whl", DIGEST_A);
    meta.put_upload(
        "hosted",
        "demo",
        "demo-1.0-py3-none-any.whl",
        crate::to_json(&uploaded).as_bytes(),
    )
    .unwrap();
    let mut index = hosted_index();
    index.policy = Policy::default().with_capabilities(
        crate::policy::compile_capabilities(&crate::policy::PypiPolicyConfig {
            block_package_types: vec![crate::policy::PackageType::Wheel],
            ..crate::policy::PypiPolicyConfig::default()
        })
        .unwrap(),
    );
    let mut output = Vec::new();
    policy_dry_run(&meta, &[index], None, None, &mut output).unwrap();
    assert!(String::from_utf8(output).unwrap().contains("package-type"));
}

#[test]
fn test_policy_dry_run_accepts_allowed_uploads() {
    let (_dir, meta) = store();
    let uploaded = upload_record("demo-1.0.tar.gz", DIGEST_A);
    meta.put_upload(
        "hosted",
        "demo",
        "demo-1.0.tar.gz",
        crate::to_json(&uploaded).as_bytes(),
    )
    .unwrap();
    let mut output = Vec::new();
    policy_dry_run(&meta, &[hosted_index()], None, None, &mut output).unwrap();
    assert!(output.is_empty());
}

#[test]
fn test_purge_project_covers_preserved_reference_shapes() {
    let (_dir, meta) = store();
    seed_valid_page(&meta);
    let body = format!(
        r#"{{"meta":{{"api-version":"1.1"}},"name":"other","versions":["1.0"],"files":[{{"filename":"no-hash.whl","size":11,"url":"u","hashes":{{}},"yanked":false}},{{"filename":"other.whl","size":11,"url":"u","hashes":{{"sha256":"{}"}},"core-metadata":{{"sha256":"{}"}},"yanked":false}}]}}"#,
        "c".repeat(64),
        "d".repeat(64),
    );
    meta.put_index(
        "pypi/other",
        &CachedIndex {
            source: None,
            last_modified: None,
            etag: None,
            last_serial: None,
            fetched_at_unix: 0,
            content_type: Some("application/json".to_owned()),
            fresh_secs: None,
            body: body.into_bytes(),
        },
    )
    .unwrap();
    purge_project(&meta, "pypi", "flask", false).unwrap();

    meta.put_index("pypi/plain", &CachedIndex {
        source: None,
        last_modified: None,
        etag: None,
        last_serial: None,
        fetched_at_unix: 0,
        content_type: Some("application/json".to_owned()),
        fresh_secs: None,
        body: format!(
            r#"{{"meta":{{"api-version":"1.1"}},"name":"plain","versions":["1.0"],"files":[{{"filename":"plain.whl","size":11,"url":"u","hashes":{{"sha256":"{}"}},"yanked":false}}]}}"#,
            "e".repeat(64)
        ).into_bytes(),
    }).unwrap();
    purge_project(&meta, "pypi", "flask", false).unwrap();

    meta.put_driver_value("pypi\u{0}i\u{0}pypi/broken", b"bad").unwrap();
    assert!(purge_project(&meta, "pypi", "flask", false).is_err());
}

#[test]
fn test_fsck_reports_decodable_invalid_project_details() {
    let (dir, meta) = store();
    seed_undecodable_detail(&meta, "pypi/demo");
    let blobs = BlobStore::new(dir.path().join("blobs")).into();
    let mut output = Vec::new();
    assert_eq!(
        fsck_metadata(&meta, &blobs, &audited_fixture(), &mut output).unwrap(),
        1
    );
    assert!(String::from_utf8(output).unwrap().contains("invalid project detail"));
}

fn not_utf8_reason(table: char, key: &str) -> String {
    MetaError::DriverRecordUtf8 {
        key: format!("pypi\u{0}{table}\u{0}{key}"),
        source: String::from_utf8(vec![0xff, 0xfe]).unwrap_err(),
    }
    .to_string()
}

#[rstest]
#[case::file_url('f', "not-hex", "file-url", false)]
#[case::pep658('d', "not-hex", "pep658", false)]
#[case::publication('n', "pypi/demo/sha/demo-1.0.whl", "publication", false)]
#[case::project('p', "pypi/flask", "project", true)]
#[case::override_record('o', "hosted/demo/demo.whl", "override", false)]
#[case::provenance('a', "pypi/flask/sha/flask-1.0.whl", "provenance", false)]
fn test_fsck_names_a_record_it_cannot_read_and_marks_the_scan_incomplete(
    #[case] table: char,
    #[case] key: &str,
    #[case] record: &str,
    // A project row is counted by the row it sits in whether or not its value reads back, so the
    // project case leaves an index whose count row was never written, and the audit says so.
    #[case] counted: bool,
) {
    let (dir, meta) = store();
    let blobs = BlobStore::new(dir.path().join("blobs")).into();
    meta.put_driver_value(&format!("pypi\u{0}{table}\u{0}{key}"), &[0xff, 0xfe])
        .unwrap();
    let mut output = Vec::new();
    let uncounted = format!(
        "metadata\tpypi\tsummary-count\t{:?}\tcount row is absent, rows hold 1 projects and 0 uploads\n",
        "pypi\u{0}k\u{0}pypi"
    );

    let problems = fsck_metadata(&meta, &blobs, &audited_fixture(), &mut output).unwrap();

    assert_eq!(
        (problems, String::from_utf8(output).unwrap()),
        (
            1 + u64::from(counted),
            format!(
                "metadata\tpypi\t{record}\t{key}\t{}\nmetadata\tpypi\t{record}\t*\tscan incomplete\n{}",
                not_utf8_reason(table, key),
                if counted { uncounted.as_str() } else { "" }
            )
        )
    );
}

#[test]
fn test_fsck_still_checks_the_intact_rows_beside_one_it_cannot_read() {
    let (dir, meta) = store();
    let blobs = BlobStore::new(dir.path().join("blobs")).into();
    meta.put_driver_value("pypi\u{0}p\u{0}pypi/flask", b"").unwrap();
    meta.put_driver_value("pypi\u{0}p\u{0}pypi/torch", &[0xff, 0xfe])
        .unwrap();
    let mut output = Vec::new();

    let problems = fsck_metadata(&meta, &blobs, &audited_fixture(), &mut output).unwrap();

    assert_eq!(
        (problems, String::from_utf8(output).unwrap()),
        (
            3,
            format!(
                "metadata\tpypi\tproject\tpypi/flask\tinvalid record\nmetadata\tpypi\tproject\tpypi/torch\t{}\nmetadata\tpypi\tproject\t*\tscan incomplete\nmetadata\tpypi\tsummary-count\t{:?}\tcount row is absent, rows hold 2 projects and 0 uploads\n",
                not_utf8_reason('p', "pypi/torch"),
                "pypi\u{0}k\u{0}pypi"
            )
        )
    );
}

#[test]
fn test_counting_records_refuses_a_store_holding_a_row_it_cannot_read() {
    let (_dir, meta) = store();
    meta.put_driver_value("pypi\u{0}f\u{0}pypi/flask/not-hex", &[0xff, 0xfe])
        .unwrap();

    assert_eq!(
        cache_record_counts(&meta).unwrap_err(),
        not_utf8_reason('f', "pypi/flask/not-hex")
    );
}

#[test]
fn test_collecting_referenced_digests_refuses_a_row_it_cannot_read() {
    let (_dir, meta) = store();
    meta.put_driver_value("pypi\u{0}f\u{0}pypi/flask/not-hex", &[0xff, 0xfe])
        .unwrap();

    assert_eq!(
        referenced_blob_digests(&meta).unwrap_err(),
        not_utf8_reason('f', "pypi/flask/not-hex")
    );
}

/// A hosted publication's provenance is scoped to that publication, so purging a cached project that
/// happens to share the digest leaves it alone.
#[test]
fn test_purge_project_leaves_a_hosted_publication_its_provenance() {
    let (_dir, meta) = store();
    seed_valid_page(&meta);
    let digest = Digest::of(b"wheel");
    meta.put_upload(
        "hosted",
        "flask",
        "flask-1.0.whl",
        crate::to_json(&upload_record("flask-1.0.whl", digest.as_str())).as_bytes(),
    )
    .unwrap();
    meta.put_provenance(
        "hosted",
        "flask",
        digest.as_str(),
        "flask-1.0.whl",
        provenance_bundle(DIGEST_B),
    )
    .unwrap();

    purge_project(&meta, "pypi", "flask", true).unwrap();

    assert!(
        meta.get_provenance("hosted", "flask", digest.as_str(), "flask-1.0.whl")
            .unwrap()
            .is_some()
    );
}

/// Preserving what other projects still advertise must not stop a purge removing what nothing does.
#[test]
fn test_purge_project_still_removes_a_digest_no_one_else_advertises() {
    let (_dir, meta) = store();
    seed_valid_page(&meta);
    let digest = Digest::of(b"wheel");

    purge_project(&meta, "pypi", "flask", true).unwrap();

    assert_eq!(meta.get_file_url("pypi", "flask", digest.as_str()).unwrap(), None);
}

#[test]
fn test_repair_reports_and_drops_a_source_row_that_names_no_publication() {
    let (_dir, meta) = store();
    seed_valid_page(&meta);
    meta.put_driver_value(
        &format!("pypi\0f\0{}", Digest::of(b"wheel").as_str()),
        b"https://legacy.example/flask.whl\npypi",
    )
    .unwrap();
    let mut out = Vec::new();

    let problems = repair_metadata(&meta, &[cached_index()], &mut out).unwrap();

    assert!(problems.actionable >= 1);
    assert!(String::from_utf8(out).unwrap().contains("\tfile-url\t"));
    assert_eq!(
        crate::store::drop_legacy_file_sources(&meta).unwrap(),
        0,
        "the sweep is idempotent"
    );
}

#[rstest]
#[case::index("pypi\0i\0cached/flask", b"invalid page", "index", true)]
#[case::unowned_index("pypi\0i\0unknown/flask", b"invalid page", "index", false)]
#[case::file_url("pypi\0f\0legacy", b"url\ncached", "file-url", true)]
#[case::unowned_file_url("pypi\0f\0unknown/flask/invalid", b"invalid", "file-url", false)]
#[case::owned_file_url("pypi\0f\0cached/flask/invalid", b"url\ncached", "file-url", false)]
#[case::pep658("pypi\0d\0invalid", b"invalid", "pep658", true)]
#[case::publication("pypi\0n\0cached/flask/invalid/flask.whl", b"invalid", "publication", false)]
#[case::publication_digest(
    "pypi\0n\0cached/flask/invalid/flask.whl",
    b"url\ninvalid\ncached",
    "publication",
    false
)]
#[case::project("pypi\0p\0cached/flask", b"", "project", true)]
#[case::unowned_project("pypi\0p\0invalid", b"", "project", false)]
#[case::unnamed_project("pypi\0p\0cached/", b"Flask", "project", false)]
#[case::upload("pypi\0u\0hosted/flask/flask.whl", b"invalid", "upload", false)]
#[case::override_row("pypi\0o\0hosted/flask/flask.whl", b"invalid", "override", false)]
#[case::provenance("pypi\0a\0hosted/flask/invalid/flask.whl", b"invalid\n16", "provenance", false)]
fn test_repair_assigns_each_corrupt_namespace_a_disposition(
    #[case] key: &str,
    #[case] value: &[u8],
    #[case] namespace: &str,
    #[case] actionable: bool,
) {
    let (_dir, meta) = store();
    meta.put_driver_value(key, value).unwrap();
    let indexes = [cached_index(), hosted_index()];
    let mut preview = Vec::new();

    let planned = preview_metadata_repair(&meta, &indexes, &mut preview).unwrap();

    assert!(planned.actionable + planned.report_only > 0);
    let preview = String::from_utf8(preview).unwrap();
    let disposition = if actionable { "remove" } else { "report-only" };
    assert!(
        preview
            .lines()
            .any(|line| { line.contains(&format!("\t{namespace}\t")) && line.contains(&format!("\t{disposition}\t")) })
    );

    repair_metadata(&meta, &indexes, &mut Vec::new()).unwrap();

    assert_eq!(meta.get_driver_value(key).unwrap().is_none(), actionable);
}

#[test]
fn test_repair_reports_an_invalid_metadata_digest_for_a_valid_artifact() {
    let (_dir, meta) = store();
    let key = format!("pypi\0d\0{DIGEST_A}");
    meta.put_driver_value(&key, b"invalid").unwrap();
    let mut preview = Vec::new();

    let planned = preview_metadata_repair(&meta, &[cached_index()], &mut preview).unwrap();

    assert_eq!(planned.report_only, 1);
    assert!(String::from_utf8(preview).unwrap().contains("\tpep658\t"));
    repair_metadata(&meta, &[cached_index()], &mut Vec::new()).unwrap();
    assert_eq!(meta.get_driver_value(&key).unwrap(), Some(b"invalid".to_vec()));
}

#[rstest]
#[case::empty_publication(format!("pypi\0n\0cached/flask/{DIGEST_A}/flask.whl"), Vec::new())]
#[case::override_row(
    "pypi\0o\0hosted/flask/flask.whl".to_owned(),
    crate::store::FileOverride::default().encode().into_bytes()
)]
fn test_repair_accepts_a_valid_record(#[case] key: String, #[case] value: Vec<u8>) {
    let (_dir, meta) = store();
    meta.put_driver_value(&key, &value).unwrap();

    let planned = preview_metadata_repair(&meta, &[cached_index(), hosted_index()], &mut Vec::new()).unwrap();

    assert_eq!(planned, peryx_driver::serving::MetadataRepairCounts::default());
}

/// The repair pass reads ownership the same way `fsck` does: an index outside `PyPI`, or one that owns
/// no rows, never delimits a source key, so the row stays an unowned one to report.
#[rstest]
#[case::another_ecosystem(oci_hosted_index())]
#[case::virtual_index(virtual_index())]
fn test_repair_splits_a_source_key_only_on_an_index_owning_rows(#[case] index: Index) {
    let (_dir, meta) = store();
    let key = format!("pypi\0f\0org/team/flask/{DIGEST_A}");
    meta.put_driver_value(&key, b"u\norg/team").unwrap();
    let mut preview = Vec::new();

    let planned = preview_metadata_repair(&meta, &[named_org_team(index)], &mut preview).unwrap();

    assert_eq!(planned.report_only, 1, "{}", String::from_utf8_lossy(&preview));
}

#[rstest]
#[case::invalid_key("invalid", upload_value(Some(DIGEST_A)))]
#[case::missing_digest("hosted/flask/flask.whl", upload_value(None))]
#[case::invalid_digest("hosted/flask/flask.whl", upload_value(Some("invalid")))]
#[case::empty_project("hosted//flask.whl", upload_value(Some(DIGEST_A)))]
fn test_repair_reports_each_invalid_upload_field(#[case] upload_key: &str, #[case] value: Vec<u8>) {
    let (_dir, meta) = store();
    let key = format!("pypi\0u\0{upload_key}");
    meta.put_driver_value(&key, &value).unwrap();
    let mut preview = Vec::new();

    let planned = preview_metadata_repair(&meta, &[hosted_index()], &mut preview).unwrap();

    assert_eq!(planned.report_only, 1);
    assert!(String::from_utf8(preview).unwrap().contains("\tupload\t"));
    repair_metadata(&meta, &[hosted_index()], &mut Vec::new()).unwrap();
    assert_eq!(meta.get_driver_value(&key).unwrap(), Some(value));
}

#[rstest]
#[case::file_url(
    format!("pypi\0f\0cached/flask/{DIGEST_A}"),
    false
)]
#[case::publication(
    format!("pypi\0n\0cached/flask/{DIGEST_A}/flask.whl"),
    false
)]
#[case::project("pypi\0p\0cached/flask".to_owned(), true)]
#[case::upload("pypi\0u\0hosted/flask/flask.whl".to_owned(), false)]
#[case::override_row("pypi\0o\0hosted/flask/flask.whl".to_owned(), false)]
#[case::provenance("pypi\0a\0hosted/flask/invalid/flask.whl".to_owned(), false)]
fn test_repair_handles_non_utf8_values(#[case] key: String, #[case] actionable: bool) {
    let (_dir, meta) = store();
    meta.put_driver_value(&key, &[0xff, 0xfe]).unwrap();
    let indexes = [cached_index(), hosted_index()];
    let mut preview = Vec::new();

    preview_metadata_repair(&meta, &indexes, &mut preview).unwrap();

    let disposition = if actionable { "\tremove\t" } else { "\treport-only\t" };
    assert!(String::from_utf8(preview).unwrap().contains(disposition));
    repair_metadata(&meta, &indexes, &mut Vec::new()).unwrap();
    assert_eq!(meta.get_driver_value(&key).unwrap().is_none(), actionable);
}

#[rstest]
#[case::preview(false)]
#[case::apply(true)]
fn test_repair_output_failure_never_reports_an_unapplied_change(#[case] apply: bool) {
    let (_dir, meta) = store();
    let key = "pypi\0d\0invalid";
    meta.put_driver_value(key, b"invalid").unwrap();
    let mut closed: &mut [u8] = &mut [];

    let result = if apply {
        repair_metadata(&meta, &[cached_index()], &mut closed)
    } else {
        preview_metadata_repair(&meta, &[cached_index()], &mut closed)
    };

    assert!(result.is_err());
    assert_eq!(meta.get_driver_value(key).unwrap().is_none(), apply);
}

#[test]
fn test_repair_preserves_an_override_from_a_newer_schema() {
    let (_dir, meta) = store();
    let key = "pypi\0o\0hosted/flask/flask.whl";
    let mut value = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(
        &crate::store::FileOverride::default().encode(),
    )
    .unwrap();
    value.insert("future-field".to_owned(), true.into());
    let value = serde_json::to_vec(&value).unwrap();
    meta.put_driver_value(key, &value).unwrap();
    let mut preview = Vec::new();

    preview_metadata_repair(&meta, &[hosted_index()], &mut preview).unwrap();

    assert!(String::from_utf8(preview).unwrap().contains("\toverride\t"));
    repair_metadata(&meta, &[hosted_index()], &mut Vec::new()).unwrap();
    assert_eq!(meta.get_driver_value(key).unwrap(), Some(value));
}

#[test]
fn test_repair_preview_does_not_mix_a_concurrent_write_into_its_report() {
    struct ConcurrentWriter {
        output: Vec<u8>,
        start: Option<std::sync::mpsc::Sender<()>>,
        done: std::sync::mpsc::Receiver<()>,
    }

    impl std::io::Write for ConcurrentWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if let Some(start) = self.start.take() {
                start.send(()).unwrap();
                self.done.recv().unwrap();
            }
            self.output.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let (_dir, meta) = store();
    meta.put_driver_value("pypi\0f\0legacy", b"url\ncached").unwrap();
    let (start_tx, start_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let writer_meta = meta.clone();
    let writer = std::thread::spawn(move || {
        start_rx.recv().unwrap();
        writer_meta.put_driver_value("pypi\0d\0invalid", b"invalid").unwrap();
        done_tx.send(()).unwrap();
    });
    let mut output = ConcurrentWriter {
        output: Vec::new(),
        start: Some(start_tx),
        done: done_rx,
    };

    preview_metadata_repair(&meta, &[cached_index()], &mut output).unwrap();
    std::io::Write::flush(&mut output).unwrap();
    writer.join().unwrap();

    let output = String::from_utf8(output.output).unwrap();
    assert!(output.contains("\tfile-url\t"));
    assert!(!output.contains("\tpep658\t"));
    let mut next = Vec::new();
    preview_metadata_repair(&meta, &[cached_index()], &mut next).unwrap();
    assert!(String::from_utf8(next).unwrap().contains("\tpep658\t"));
}

#[test]
fn test_repair_preserves_active_rows_that_are_not_safely_derivable() {
    let (_dir, meta) = store();
    seed_valid_page(&meta);
    let digest = Digest::of(b"wheel");
    let file_key = format!("pypi\0f\0pypi/flask/{}", digest.as_str());
    let publication_key = format!("pypi\0n\0pypi/flask/{}/flask-1.0.whl", digest.as_str());
    meta.put_driver_value(&file_key, b"invalid").unwrap();
    meta.put_driver_value(&publication_key, b"invalid").unwrap();
    meta.put_driver_value("pypi\0p\0pypi/flask", b"").unwrap();
    let mut preview = Vec::new();

    let planned = preview_metadata_repair(&meta, &[cached_pypi_index()], &mut preview).unwrap();

    assert_eq!(planned.report_only, 2);
    assert!(planned.actionable >= 1);
    assert_eq!(String::from_utf8(preview).unwrap().matches("\tremove\t").count(), 1);

    repair_metadata(&meta, &[cached_pypi_index()], &mut Vec::new()).unwrap();

    assert_eq!(meta.get_driver_value(&file_key).unwrap(), Some(b"invalid".to_vec()));
    assert_eq!(
        meta.get_driver_value(&publication_key).unwrap(),
        Some(b"invalid".to_vec())
    );
    assert!(meta.get_project("pypi", "flask").unwrap().is_none());
}

#[test]
fn test_repair_removes_only_an_invalid_cached_page() {
    let (dir, meta) = store();
    seed_valid_page(&meta);
    let digest = Digest::of(b"wheel");
    let file_key = format!("pypi\0f\0pypi/flask/{}", digest.as_str());
    let publication_key = format!("pypi\0n\0pypi/flask/{}/flask-1.0.whl", digest.as_str());
    let file_value = meta.get_driver_value(&file_key).unwrap().unwrap();
    let publication_value = meta.get_driver_value(&publication_key).unwrap().unwrap();
    let project_value = meta.get_driver_value("pypi\0p\0pypi/flask").unwrap().unwrap();
    meta.put_driver_value("pypi\0i\0pypi/flask", b"invalid").unwrap();

    repair_metadata(&meta, &[cached_pypi_index()], &mut Vec::new()).unwrap();

    assert!(meta.get_driver_value("pypi\0i\0pypi/flask").unwrap().is_none());
    assert_eq!(meta.get_driver_value(&file_key).unwrap(), Some(file_value));
    assert_eq!(
        meta.get_driver_value(&publication_key).unwrap(),
        Some(publication_value)
    );
    assert_eq!(
        meta.get_driver_value("pypi\0p\0pypi/flask").unwrap(),
        Some(project_value)
    );
    assert!(meta.get_driver_value("pypi\0x\0pypi/flask").unwrap().is_none());
    let blobs: BlobStorage = BlobStore::new(dir.path().join("blobs")).into();
    let mut audit = Vec::new();
    assert_eq!(
        fsck_metadata(&meta, &blobs, &[cached_pypi_index()], &mut audit).unwrap(),
        0
    );
    assert!(audit.is_empty());
}

#[rstest]
#[case::project("pypi\0p\0foo/bar/demo", b"")]
#[case::publication("pypi\0n\0foo/bar/demo/invalid/demo.whl", b"invalid")]
fn test_repair_uses_the_longest_index_name_for_ownership(#[case] key: &str, #[case] value: &[u8]) {
    let (_dir, meta) = store();
    meta.put_driver_value(key, value).unwrap();
    let indexes = [
        cached_index_with_name("foo"),
        Index {
            name: "foo/bar".to_owned(),
            route: "foo/bar".to_owned(),
            ..hosted_index()
        },
    ];

    let mut preview = Vec::new();
    preview_metadata_repair(&meta, &indexes, &mut preview).unwrap();

    assert!(String::from_utf8(preview).unwrap().contains("\treport-only\t"));
    repair_metadata(&meta, &indexes, &mut Vec::new()).unwrap();
    assert_eq!(meta.get_driver_value(key).unwrap(), Some(value.to_vec()));
}

#[test]
fn test_repair_accepts_a_file_source_owned_by_a_slash_bearing_index() {
    let (dir, meta) = store();
    let key = format!("pypi\0f\0foo/bar/demo/{DIGEST_A}");
    let value = b"https://files.example/demo.whl\nfoo/bar";
    meta.put_driver_value(&key, value).unwrap();
    let index = cached_index_with_name("foo/bar");
    let blobs: BlobStorage = BlobStore::new(dir.path().join("blobs")).into();
    let mut audit = Vec::new();

    assert_eq!(
        fsck_metadata(&meta, &blobs, std::slice::from_ref(&index), &mut audit).unwrap(),
        0
    );
    assert!(audit.is_empty());
    let planned = preview_metadata_repair(&meta, std::slice::from_ref(&index), &mut Vec::new()).unwrap();
    assert_eq!(planned, peryx_driver::serving::MetadataRepairCounts::default());
    repair_metadata(&meta, &[index], &mut Vec::new()).unwrap();
    assert_eq!(meta.get_driver_value(&key).unwrap(), Some(value.to_vec()));
}

#[test]
fn test_repair_leaves_a_valid_upload_with_a_missing_blob_for_operator_recovery() {
    let (dir, meta) = store();
    let blobs: BlobStorage = BlobStore::new(dir.path().join("blobs")).into();
    meta.put_upload(
        "hosted",
        "flask",
        "flask-1.0.whl",
        crate::to_json(&upload_record("flask-1.0.whl", DIGEST_A)).as_bytes(),
    )
    .unwrap();
    let mut fsck = Vec::new();
    let mut preview = Vec::new();

    assert_eq!(fsck_metadata(&meta, &blobs, &[hosted_index()], &mut fsck).unwrap(), 1);
    let planned = preview_metadata_repair(&meta, &[hosted_index()], &mut preview).unwrap();

    assert_eq!(planned.report_only, 0);
    assert!(!String::from_utf8(preview).unwrap().contains("\tupload\t"));
    assert!(meta.get_upload("hosted", "flask", "flask-1.0.whl").unwrap().is_some());
}

#[test]
fn test_repair_can_resume_after_the_metadata_phase_fails() {
    let observed_boundary = (0..32).rev().any(|fail_after| {
        let (_dir, meta) = store();
        meta.put_driver_value("pypi\0p\0cached/flask", b"Flask").unwrap();
        meta.put_driver_value("pypi\0f\0legacy", b"url\ncached").unwrap();
        meta.fail_driver_prefix_scan_after(fail_after);
        let mut output = Vec::new();

        let result = repair_metadata(&meta, &[cached_index()], &mut output);
        meta.fail_driver_prefix_scan_after(usize::MAX);
        if result.is_err()
            && crate::store::audit_summary_rows(
                &meta,
                &[crate::store::AuditedIndex {
                    name: "cached",
                    local: true,
                }],
            )
            .unwrap()
            .is_empty()
            && meta.get_driver_value("pypi\0f\0legacy").unwrap().is_some()
        {
            assert!(output.is_empty());
            repair_metadata(&meta, &[cached_index()], &mut output).unwrap();
            assert!(meta.get_driver_value("pypi\0f\0legacy").unwrap().is_none());
            true
        } else {
            false
        }
    });

    assert!(observed_boundary);
}

#[test]
fn test_repair_metadata_transaction_is_atomic_across_backend_failures() {
    let mut failed = 0_u32;
    for fail_after in 0..96 {
        let (pages, fault) = peryx_test_support::fault::backend();
        let meta = MetaStore::open_backend(peryx_test_support::fault::faulted(&pages, &fault)).unwrap();
        meta.put_driver_value("pypi\0f\0legacy", b"url\ncached").unwrap();
        meta.put_driver_value("pypi\0d\0invalid", b"invalid").unwrap();
        drop(meta);
        let meta = MetaStore::reopen_backend(peryx_test_support::fault::faulted(&pages, &fault)).unwrap();
        fault.arm(fail_after);
        let repaired = repair_metadata(&meta, &[cached_index()], &mut Vec::new());
        fault.disable();
        drop(meta);

        let meta = MetaStore::reopen_backend(peryx_test_support::fault::faulted(&pages, &fault)).unwrap();
        let remaining = ["pypi\0f\0legacy", "pypi\0d\0invalid"]
            .into_iter()
            .filter(|key| meta.get_driver_value(key).unwrap().is_some())
            .count();
        if repaired.is_ok() {
            assert_eq!(remaining, 0);
        } else {
            assert!(
                matches!(remaining, 0 | 2),
                "failure after {fail_after} operations committed half the repair"
            );
            failed += 1;
        }
    }

    assert!(failed > 0, "no backend failure reached the repair");
}

/// The check sums problems across nine scans, so a failure in any of them must not come back as a
/// smaller count. A short problem count is a false all-clear in the same way a short defect list is:
/// an operator reads "three problems" as the whole truth and fixes three.
///
/// A store handle does not survive its own injected failure, so each step reopens the retained pages
/// rather than reusing one handle.
#[test]
fn fsck_metadata_never_reports_fewer_problems_than_exist() {
    let dir = tempfile::tempdir().unwrap();
    let blobs: BlobStorage = BlobStore::new(dir.path().join("blobs")).into();
    let (pages, fault) = peryx_test_support::fault::backend();
    let meta = MetaStore::open_backend(peryx_test_support::fault::faulted(&pages, &fault)).unwrap();
    meta.put_driver_value("pypi\u{0}i\u{0}pypi/flask", b"garbage").unwrap();
    meta.put_driver_value("pypi\u{0}f\u{0}not-hex", b"u\npypi").unwrap();
    meta.put_driver_value("pypi\u{0}d\u{0}not-hex", b"u\nm\npypi").unwrap();
    meta.put_driver_value("pypi\u{0}p\u{0}pypi/flask", b"").unwrap();
    meta.put_upload("pypi", "flask", "flask-1.0.whl", b"not json").unwrap();
    meta.put_driver_value("pypi\u{0}o\u{0}pypi/flask/flask-1.0.whl", b"bogus")
        .unwrap();
    meta.put_driver_value("pypi\u{0}a\u{0}pypi/flask/not-hex/flask-1.0.whl", b"abc\n16")
        .unwrap();
    meta.put_provenance("pypi", "flask", DIGEST_A, "flask-1.0.whl", provenance_bundle(DIGEST_B))
        .unwrap();
    let mut whole_report = Vec::new();
    let whole = fsck_metadata(&meta, &blobs, &audited_fixture(), &mut whole_report).unwrap();
    assert_eq!(whole, 8, "{}", String::from_utf8_lossy(&whole_report));
    drop(meta);

    let mut failed = 0_u32;
    for fail_after in 0..256 {
        let meta = MetaStore::reopen_backend(peryx_test_support::fault::faulted(&pages, &fault)).unwrap();
        fault.arm(fail_after);
        let mut report = Vec::new();
        let counted = fsck_metadata(&meta, &blobs, &audited_fixture(), &mut report);
        fault.disable();
        match counted {
            Ok(problems) => assert_eq!(
                (problems, report),
                (whole, whole_report.clone()),
                "injecting after {fail_after} reads reported short"
            ),
            Err(_) => failed += 1,
        }
    }

    assert!(failed > 0, "no injection point reached the checks");
}

/// A purge decides what to keep from a scan of the project's file rows, so a scan that fails partway
/// must not hand back a smaller preserved set. Preserving less means removing more, and the rows it
/// would take are the ones a project populated by a catalog sync needs for a cold download.
///
/// The run is a dry run, so the store is identical at every injection point and any difference in
/// the report comes from the scan rather than from what an earlier step deleted.
///
/// A store handle does not survive its own injected failure, so each step reopens the retained pages
/// rather than reusing one handle.
#[test]
fn purge_project_never_preserves_less_than_it_should() {
    let (pages, fault) = peryx_test_support::fault::backend();
    let meta = MetaStore::open_backend(peryx_test_support::fault::faulted(&pages, &fault)).unwrap();
    seed_valid_page(&meta);
    let uploaded = upload_record("other-1.0.tar.gz", Digest::of(b"preserved upload").as_str());
    meta.put_upload(
        "hosted",
        "other",
        "other-1.0.tar.gz",
        crate::to_json(&uploaded).as_bytes(),
    )
    .unwrap();
    let whole = super::purge_project(&meta, "pypi", "flask", false).unwrap();
    drop(meta);

    let mut failed = 0_u32;
    for fail_after in 0..192 {
        let meta = MetaStore::reopen_backend(peryx_test_support::fault::faulted(&pages, &fault)).unwrap();
        fault.arm(fail_after);
        let report = super::purge_project(&meta, "pypi", "flask", false);
        fault.disable();
        match report {
            Ok(got) => assert_eq!(got, whole, "injecting after {fail_after} reads preserved less"),
            Err(_) => failed += 1,
        }
    }

    assert!(failed > 0, "no injection point reached the scan");
}
