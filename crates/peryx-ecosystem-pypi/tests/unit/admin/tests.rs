use peryx_identity::IndexAcl;
use std::convert::Infallible;

use peryx_index::{Index, IndexKind};
use peryx_policy::Policy;
use peryx_storage::blob::{BlobStore, Digest};
use peryx_storage::meta::{MetaError, MetaScanError, MetaStore};

use super::{cache_pages, cache_record_counts, fsck_metadata, policy_dry_run, referenced_blob_digests};
use crate::store::{CachedIndex, PypiStore as _};

fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, meta)
}

/// A valid cached simple-index page whose body parses into a project detail.
fn seed_valid_page(meta: &MetaStore) {
    let digest = Digest::of(b"wheel");
    let metadata_digest = Digest::of(b"metadata");
    let body = format!(
        r#"{{"meta":{{"api-version":"1.1"}},"name":"flask","versions":["1.0"],"files":[{{"filename":"flask-1.0.whl","url":"https://files/flask.whl","hashes":{{"sha256":"{d}"}},"core-metadata":{{"sha256":"{m}"}},"yanked":false}}]}}"#,
        d = digest.as_str(),
        m = metadata_digest.as_str(),
    );
    meta.put_index(
        "pypi/flask",
        &CachedIndex {
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
    meta.put_file_url(digest.as_str(), "https://files/flask.whl", "pypi")
        .unwrap();
    meta.put_metadata(
        digest.as_str(),
        "https://files/flask.whl.metadata",
        metadata_digest.as_str(),
        "pypi",
    )
    .unwrap();
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
fn test_cache_pages_lists_the_stored_pages_split_by_index() {
    let (_dir, meta) = store();
    seed_valid_page(&meta);
    let pages = cache_pages(&meta, &["pypi"]).unwrap();
    assert_eq!(pages.len(), 1);
    assert_eq!((pages[0].index.as_str(), pages[0].project.as_str()), ("pypi", "flask"));
}

#[test]
fn test_cache_record_counts_counts_each_record_kind() {
    let (_dir, meta) = store();
    seed_valid_page(&meta);
    meta.put_upload("pypi", "flask", "flask-1.0.whl", br#"{"version":"1.0"}"#)
        .unwrap();
    meta.put_override("pypi", "flask", "flask-1.0.whl", "yanked", 0)
        .unwrap();
    meta.put_provenance(&"a".repeat(64), &"b".repeat(64), 16).unwrap();
    let counts: std::collections::HashMap<String, u64> = cache_record_counts(&meta).unwrap().into_iter().collect();
    assert_eq!(counts["file_url_records"], 1);
    assert_eq!(counts["metadata_records"], 1);
    assert_eq!(counts["project_records"], 1);
    assert_eq!(counts["upload_records"], 1);
    assert_eq!(counts["override_records"], 1);
    assert_eq!(counts["provenance_records"], 1);
}

#[test]
fn test_referenced_blob_digests_rejects_a_corrupt_file_url_record() {
    let (_dir, meta) = store();
    // A file-URL row keyed by a non-hex digest is a corrupt record. `pypi\0f\0` is its namespace.
    meta.put_driver_value("pypi\u{0}f\u{0}not-hex", b"https://files/x\npypi")
        .unwrap();
    assert!(referenced_blob_digests(&meta).is_err());
}

#[test]
fn test_referenced_blob_digests_rejects_a_corrupt_metadata_record() {
    let (_dir, meta) = store();
    // A PEP 658 row keyed by a non-hex digest. `pypi\0d\0` is the metadata-sidecar namespace.
    meta.put_driver_value("pypi\u{0}d\u{0}not-hex", b"https://files/x.metadata\nabc\npypi")
        .unwrap();
    assert!(referenced_blob_digests(&meta).is_err());
}

#[test]
fn test_referenced_blob_digests_rejects_a_corrupt_upload_record() {
    let (_dir, meta) = store();
    meta.put_upload("pypi", "flask", "flask-1.0.whl", b"not json").unwrap();
    assert!(referenced_blob_digests(&meta).is_err());
}

#[test]
fn test_referenced_blob_digests_includes_the_provenance_blob() {
    let (_dir, meta) = store();
    let provenance_blob = "c".repeat(64);
    meta.put_provenance(&"a".repeat(64), &provenance_blob, 16).unwrap();
    assert!(referenced_blob_digests(&meta).unwrap().contains(&provenance_blob));
}

#[test]
fn test_referenced_blob_digests_rejects_a_corrupt_provenance_record() {
    let (_dir, meta) = store();
    // A provenance row keyed by a non-hex digest. `pypi\0a\0` is the provenance namespace.
    meta.put_driver_value("pypi\u{0}a\u{0}not-hex", b"abc\n16").unwrap();
    assert!(referenced_blob_digests(&meta).is_err());
}

#[test]
fn test_fsck_metadata_reports_every_invalid_record_kind() {
    let (dir, meta) = store();
    let blobs = BlobStore::new(dir.path().join("blobs")).into();
    meta.put_driver_value("pypi\u{0}i\u{0}pypi/flask", b"garbage").unwrap();
    meta.put_driver_value("pypi\u{0}f\u{0}not-hex", b"u\npypi").unwrap();
    meta.put_driver_value("pypi\u{0}d\u{0}not-hex", b"u\nm\npypi").unwrap();
    meta.put_driver_value("pypi\u{0}p\u{0}pypi/flask", b"").unwrap();
    meta.put_upload("pypi", "flask", "flask-1.0.whl", b"not json").unwrap();
    meta.put_override("pypi", "flask", "flask-1.0.whl", "bogus", 0).unwrap();
    meta.put_driver_value("pypi\u{0}a\u{0}not-hex", b"abc\n16").unwrap();
    // A valid provenance row exercises the fsck scan's accept path alongside the invalid one.
    meta.put_provenance(&"a".repeat(64), &"b".repeat(64), 16).unwrap();
    let mut out = Vec::new();
    let problems = fsck_metadata(&meta, &blobs, &mut out).unwrap();
    assert_eq!(problems, 7, "{}", String::from_utf8_lossy(&out));
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

/// A framed page whose body decodes but is not a valid project detail, so `parse_detail` fails.
fn seed_undecodable_detail(meta: &MetaStore, key: &str) {
    meta.put_index(
        key,
        &CachedIndex {
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
    let report = super::purge_project(&meta, "pypi", "flask", false).unwrap();
    assert_eq!(report.project, "flask");
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
    // A second, non-target page whose body is not a detail: scanned as a preserved reference and
    // rejected.
    seed_undecodable_detail(&meta, "pypi/other");
    assert!(super::purge_project(&meta, "pypi", "flask", false).is_err());
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

#[test]
fn test_policy_dry_run_skips_uploads_it_cannot_attribute() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    // An upload on an index no configured index names: attributed by the fallback split, then
    // skipped because it matches no index.
    meta.put_upload("ghost", "proj", "file.whl", br#"{"version":"1.0"}"#)
        .unwrap();
    // An upload on a configured index, filtered out by a project filter that does not match it.
    meta.put_upload("hosted", "flask", "flask-1.0.whl", br#"{"version":"1.0"}"#)
        .unwrap();
    // A corrupt upload row whose key carries no project/filename split is skipped entirely. The
    // `pypi\0u\0` prefix is the on-disk upload namespace.
    meta.put_driver_value("pypi\u{0}u\u{0}noslashkey", b"x").unwrap();

    let indexes = [hosted_index()];
    let mut out = Vec::new();
    policy_dry_run(&meta, &indexes, None, Some("other"), &mut out).unwrap();

    // No configured, unfiltered upload reaches a policy check, so nothing is written.
    assert_eq!(String::from_utf8(out).unwrap(), "");
}
