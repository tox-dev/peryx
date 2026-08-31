use peryx_identity::IndexAcl;
use std::convert::Infallible;

use peryx_index::{Index, IndexKind};
use peryx_policy::{Policy, PolicyConfig};
use peryx_storage::blob::{BlobStorage, BlobStore, Digest};
use peryx_storage::meta::{MetaError, MetaScanError, MetaStore};
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
        },
        trashed: None,
    }
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

    meta.put_driver_value("pypi\u{0}f\u{0}not-hex", b"https://files/x\npypi")
        .unwrap();
    assert!(referenced_blob_digests(&meta).is_err());
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
#[case::provenance(format!("not-hex\n16"))]
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
    let problems = fsck_metadata(&meta, &blobs, &mut out).unwrap();
    assert_eq!(problems, 7, "{}", String::from_utf8_lossy(&out));
}

#[rstest]
#[case::pep658_artifact('d', "not-hex", format!("url\n{DIGEST_B}\npypi"), "pep658")]
#[case::pep658_metadata('d', DIGEST_A, "url\nnot-hex\npypi".to_owned(), "pep658")]
#[case::project_index('p', "/flask", "Flask".to_owned(), "project")]
#[case::project_name('p', "pypi/", "Flask".to_owned(), "project")]
#[case::project_display('p', "pypi/flask", String::new(), "project")]
#[case::publication_metadata('n', "pypi/demo/sha/demo-1.0.whl", "url\nnot-hex\npypi\n".to_owned(), "publication")]
#[case::publication_truncated('n', "pypi/demo/sha/demo-1.0.whl", "url".to_owned(), "publication")]
#[case::override_filename('o', "hosted/demo/", r#"{"hidden":true,"yanked":false}"#.to_owned(), "override")]
#[case::override_kind('o', "hosted/demo/demo.whl", "invalid".to_owned(), "override")]
fn test_fsck_metadata_rejects_each_invalid_field(
    #[case] table: char,
    #[case] key: &str,
    #[case] value: String,
    #[case] record: &str,
) {
    let (dir, meta) = store();
    let blobs = BlobStore::new(dir.path().join("blobs")).into();
    meta.put_driver_value(&format!("pypi\u{0}{table}\u{0}{key}"), value.as_bytes())
        .unwrap();
    let mut output = Vec::new();

    assert_eq!(fsck_metadata(&meta, &blobs, &mut output).unwrap(), 1);
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

    assert_eq!(fsck_metadata(&meta, &blobs, &mut output).unwrap(), 1);
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
    assert_eq!(fsck_metadata(&meta, &blobs, &mut output).unwrap(), 2);
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("invalid key"));
    assert!(output.contains("missing blob"));
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
    assert_eq!(fsck_metadata(&meta, &blobs, &mut output).unwrap(), 1);
    assert!(String::from_utf8(output).unwrap().contains("invalid project detail"));
}
