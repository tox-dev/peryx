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
    let problems = fsck_metadata(&meta, &blobs, &audited_fixture(), &mut out).unwrap();
    // Seven damaged records, plus the count row: the project above is written straight into the store
    // while the upload goes through the write path, so the index's count row is short by one project.
    assert_eq!(problems, 8, "{}", String::from_utf8_lossy(&out));
}

#[rstest]
#[case::pep658_artifact('d', "not-hex", format!("url\n{DIGEST_B}\npypi"), "pep658", 1)]
#[case::pep658_metadata('d', DIGEST_A, "url\nnot-hex\npypi".to_owned(), "pep658", 1)]
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
    Index {
        name: "cached".to_owned(),
        route: "cached".to_owned(),
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
    let foreign = Index {
        ecosystem: peryx_core::Ecosystem::new("oci"),
        ..hosted_index()
    };
    let mut output = Vec::new();

    let problems = fsck_metadata(&meta, &blobs, &[foreign], &mut output).unwrap();

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
    meta.put_driver_value("pypi\u{0}f\u{0}not-hex", &[0xff, 0xfe]).unwrap();

    assert_eq!(cache_record_counts(&meta).unwrap_err(), not_utf8_reason('f', "not-hex"));
}

#[test]
fn test_collecting_referenced_digests_refuses_a_row_it_cannot_read() {
    let (_dir, meta) = store();
    meta.put_driver_value("pypi\u{0}f\u{0}not-hex", &[0xff, 0xfe]).unwrap();

    assert_eq!(
        referenced_blob_digests(&meta).unwrap_err(),
        not_utf8_reason('f', "not-hex")
    );
}

/// A catalog sync stores a project's files as generation rows, and those rows carry the same
/// digest-keyed source row a cached page writes. Purging a different project must not take a source
/// the surviving project still needs for a cold download.
#[test]
fn test_purge_project_keeps_a_digest_a_generation_still_serves() {
    let (_dir, meta) = store();
    seed_valid_page(&meta);
    let digest = Digest::of(b"wheel");
    let (generation, expected) = crate::store::begin_project_generation(&meta, "pypi", "django").unwrap();
    crate::store::put_project_files(
        &meta,
        "pypi",
        "django",
        generation,
        "pypi",
        None,
        &[File {
            filename: "django-1.0.whl".to_owned(),
            url: "https://files/django.whl".to_owned(),
            hashes: std::collections::BTreeMap::from([("sha256".to_owned(), digest.as_str().to_owned())]),
            requires_python: None,
            size: Some(11),
            upload_time: None,
            yanked: Yanked::No,
            core_metadata: CoreMetadata::Absent,
            dist_info_metadata: CoreMetadata::Absent,
            gpg_sig: None,
            provenance: Provenance::Absent,
        }],
    )
    .unwrap();
    crate::store::publish_project_generation(
        &meta,
        "pypi",
        "django",
        expected,
        crate::store::ProjectGeneration {
            generation,
            source: "pypi".to_owned(),
            url: "https://files/django".to_owned(),
            format: "json".to_owned(),
            etag: None,
            last_modified: None,
            last_serial: None,
            fetched_at_unix: 0,
            bytes: 1,
            files: 1,
            versions: Vec::new(),
            project_status: None,
            project_status_reason: None,
        },
    )
    .unwrap();

    purge_project(&meta, "pypi", "flask", true).unwrap();

    assert!(
        meta.get_file_url(digest.as_str()).unwrap().is_some(),
        "django still advertises this digest and needs its source"
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

    assert_eq!(meta.get_file_url(digest.as_str()).unwrap(), None);
}

/// A generation row that does not decode leaves the preserved set incomplete, so the purge refuses
/// rather than removing a source some project may still advertise.
#[test]
fn test_purge_project_reports_a_corrupt_generation_file_row() {
    let (_dir, meta) = store();
    seed_valid_page(&meta);
    meta.put_driver_value(
        "pypi\u{0}r\u{0}pypi/django/00000000000000000001/django-1.0.whl",
        b"not json",
    )
    .unwrap();

    let error = purge_project(&meta, "pypi", "flask", false).unwrap_err();

    assert!(error.contains("corrupt project file row"), "{error}");
}

/// The purged project's own generation rows must not preserve its digests, or a catalog-synced project
/// could never be purged at all.
#[test]
fn test_purge_project_ignores_the_target_projects_own_generation_rows() {
    let (_dir, meta) = store();
    seed_valid_page(&meta);
    let digest = Digest::of(b"wheel");
    meta.put_driver_value(
        "pypi\u{0}r\u{0}pypi/flask/00000000000000000001/flask-1.0.whl",
        crate::to_json(&File {
            filename: "flask-1.0.whl".to_owned(),
            url: "https://files/flask.whl".to_owned(),
            hashes: std::collections::BTreeMap::from([("sha256".to_owned(), digest.as_str().to_owned())]),
            requires_python: None,
            size: Some(11),
            upload_time: None,
            yanked: Yanked::No,
            core_metadata: CoreMetadata::Absent,
            dist_info_metadata: CoreMetadata::Absent,
            gpg_sig: None,
            provenance: Provenance::Absent,
        })
        .as_bytes(),
    )
    .unwrap();

    purge_project(&meta, "pypi", "flask", true).unwrap();

    assert_eq!(meta.get_file_url(digest.as_str()).unwrap(), None);
}
