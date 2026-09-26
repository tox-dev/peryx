use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use blake2::Blake2b256;
use blake2::Digest as _;
use peryx_storage::blob::{BlobStorage, Digest};
use peryx_storage::meta::{MetaStore, QuotaLimits};
use serde_json::{Value, json};

use super::support::{
    hex, sdist_with_license, staged_form, wheel_metadata, wheel_metadata_bytes, wheel_without_metadata,
};
use crate::PackageName;
use crate::quota::{Admission, PendingQuota, QuotaRejection, admit_upload, quota_reservation};
use crate::store::PypiStore as _;
use crate::upload::{StagedUpload, UploadStoreError, commit_publish, prepare, stage_publish, store_prepared_blocking};

const FILENAME: &str = "Flask-1.0-py3-none-any.whl";

fn attestations_field(filename: &str, sha256: &str) -> String {
    signed_attestations_field(filename, sha256, "YmFy")
}

fn signed_attestations_field(filename: &str, sha256: &str, signature: &str) -> String {
    let statement = STANDARD.encode(
        json!({
            "_type": "https://in-toto.io/Statement/v1",
            "subject": [{"name": filename, "digest": {"sha256": sha256}}],
            "predicateType": "https://docs.pypi.org/attestations/publish/v1",
            "predicate": {},
        })
        .to_string(),
    );
    json!([{
        "version": 1,
        "verification_material": {"certificate": "Zm9v", "transparency_entries": []},
        "envelope": {"statement": statement, "signature": signature},
    }])
    .to_string()
}

/// Publish `wheel` onto the `hosted` index, attesting it under `signature` when one is given.
fn publish_wheel(
    meta: &MetaStore,
    blobs: &BlobStorage,
    wheel: &[u8],
    signature: Option<&str>,
) -> Result<bool, UploadStoreError> {
    let staged = StagedUpload {
        blob: blobs.blocking().stage_bytes(wheel)?,
        blake2_256: blake2_256(wheel),
    };
    let sha = staged.blob.digest().as_str().to_owned();
    let mut form = staged_form(wheel);
    form.attestations = signature.map(|signature| signed_attestations_field(FILENAME, &sha, signature));
    let mut prepared = prepare(form, staged, "root/hosted", 1000).unwrap();
    prepared.prepare_attestations_unverified_for_storage_test().unwrap();
    store_prepared_blocking(meta, blobs, "hosted", prepared)
}

fn publish_named_wheel(
    meta: &MetaStore,
    blobs: &BlobStorage,
    wheel: &[u8],
    filename: &str,
) -> Result<bool, UploadStoreError> {
    let staged = StagedUpload {
        blob: blobs.blocking().stage_bytes(wheel)?,
        blake2_256: blake2_256(wheel),
    };
    let mut form = staged_form(wheel);
    form.filename = Some(filename.to_owned());
    store_prepared_blocking(
        meta,
        blobs,
        "hosted",
        prepare(form, staged, "root/hosted", 1000).unwrap(),
    )
}

fn unclassify_wheel(meta: &MetaStore, filename: &str) -> crate::upload::Uploaded {
    let mut uploaded: crate::upload::Uploaded =
        serde_json::from_slice(&meta.get_upload("hosted", "flask", filename).unwrap().unwrap()).unwrap();
    uploaded.imports = None;
    uploaded
}

fn put_legacy_wheel(meta: &MetaStore, filename: &str, uploaded: &crate::upload::Uploaded) {
    meta.put_upload("hosted", "flask", filename, &serde_json::to_vec(uploaded).unwrap())
        .unwrap();
}

fn blake2_256(bytes: &[u8]) -> String {
    hex(&Blake2b256::digest(bytes))
}

fn pending_quota(meta: &MetaStore, wheel: &[u8], limit: u64) -> Result<PendingQuota, QuotaRejection> {
    let project = PackageName::new("Flask");
    let digest = Digest::of(wheel);
    let request = quota_reservation(
        "hosted",
        &project,
        Some("1.0"),
        digest.as_str(),
        wheel.len() as u64,
        1000,
    );
    match admit_upload(meta, request, QuotaLimits::default(), Some(limit)).unwrap() {
        Admission::Reserved(pending) => Ok(pending),
        Admission::Rejected(rejection) => Err(rejection),
    }
}

#[test]
fn test_pending_quota_reports_the_projected_total() {
    let wheel = wheel_metadata("Flask", "1.0");
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();

    assert!(matches!(
        pending_quota(&meta, &wheel, 0),
        Err(QuotaRejection::ProjectBytes { total }) if total == wheel.len() as u64
    ));
}

#[test]
fn test_store_prepared_blocking_stages_and_records_the_provenance_bundle() {
    let wheel = wheel_metadata("Flask", "1.0");
    let dir = tempfile::tempdir().unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();

    let blob = blobs.blocking().stage_bytes(&wheel).unwrap();
    let sha = blob.digest().as_str().to_owned();
    let staged = StagedUpload {
        blob,
        blake2_256: blake2_256(&wheel),
    };
    let mut form = staged_form(&wheel);
    form.attestations = Some(attestations_field(FILENAME, &sha));

    let mut prepared = prepare(form, staged, "root/hosted", 1000).unwrap();
    prepared.prepare_attestations_unverified_for_storage_test().unwrap();
    assert!(
        prepared.provenance.is_some(),
        "attestations produce a provenance object"
    );

    let stored = store_prepared_blocking(&meta, &blobs, "hosted", prepared).unwrap();

    assert!(stored);
    let (provenance_sha, size) = meta
        .get_provenance("hosted", "flask", &sha, FILENAME)
        .unwrap()
        .expect("the provenance row is written");
    let bytes = blobs
        .blocking()
        .read_bytes(&Digest::from_hex(&provenance_sha).unwrap(), 1 << 20)
        .unwrap();
    assert_eq!(bytes.len() as u64, size, "the recorded size matches the staged blob");
    let document: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(document["version"], 1);
    assert_eq!(
        document["attestation_bundles"][0]["attestations"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn test_commit_publish_reports_the_content_and_metadata_placements() {
    let wheel = wheel_metadata("Flask", "1.0");
    let (_staged_dir, staged) = super::support::staged_upload(&wheel);
    let prepared = prepare(staged_form(&wheel), staged, "root/hosted", 1000).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));

    let publish = stage_publish(&blobs, prepared).await.unwrap();
    let published = commit_publish(&meta, "hosted", publish, None, true, None).unwrap();

    assert!(published.stored);
    assert_eq!(
        published.placements.len(),
        2,
        "a publish without attestations places the content artifact and its metadata sibling",
    );
    let content = Digest::of(&wheel);
    assert!(
        published.placements.iter().any(|(digest, _)| digest == &content),
        "the committed content is placed",
    );
    assert!(
        published.placements.iter().all(|(_, size)| *size > 0),
        "each placement carries the blob's byte length",
    );
}

#[tokio::test]
async fn test_commit_publish_adds_the_provenance_placement() {
    let wheel = wheel_metadata("Flask", "1.0");
    let (_staged_dir, staged) = super::support::staged_upload(&wheel);
    let sha = staged.blob.digest().as_str().to_owned();
    let mut form = staged_form(&wheel);
    form.attestations = Some(attestations_field(FILENAME, &sha));
    let mut prepared = prepare(form, staged, "root/hosted", 1000).unwrap();
    prepared.prepare_attestations_unverified_for_storage_test().unwrap();
    assert!(
        prepared.provenance.is_some(),
        "attestations produce a provenance object"
    );
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));

    let publish = stage_publish(&blobs, prepared).await.unwrap();
    let published = commit_publish(&meta, "hosted", publish, None, true, None).unwrap();

    assert_eq!(
        published.placements.len(),
        3,
        "an attested publish also places the provenance blob alongside the content and metadata",
    );
}

#[tokio::test]
async fn test_commit_publish_failure_exposes_no_provenance_reference() {
    let wheel = wheel_metadata("Flask", "1.0");
    let (_staged_dir, staged) = super::support::staged_upload(&wheel);
    let sha256 = staged.blob.digest().as_str().to_owned();
    let mut form = staged_form(&wheel);
    form.attestations = Some(attestations_field(FILENAME, &sha256));
    let mut prepared = prepare(form, staged, "root/hosted", 1000).unwrap();
    prepared.prepare_attestations_unverified_for_storage_test().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let publish = stage_publish(&blobs, prepared).await.unwrap();
    meta.put_upload("hosted", "flask", FILENAME, b"invalid-json").unwrap();

    assert!(matches!(
        commit_publish(&meta, "hosted", publish, None, true, None),
        Err(UploadStoreError::Parse(_))
    ));
    assert_eq!(meta.get_provenance("hosted", "flask", &sha256, FILENAME).unwrap(), None);
}

#[tokio::test]
async fn test_store_prepared_quota_releases_after_blob_storage_fails() {
    let wheel = wheel_metadata("Flask", "1.0");
    let (_staged_dir, staged) = super::support::staged_upload(&wheel);
    let prepared = prepare(staged_form(&wheel), staged, "root/hosted", 1000).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let invalid_root = dir.path().join("not-a-directory");
    std::fs::write(&invalid_root, b"file").unwrap();
    let blobs = BlobStorage::filesystem(invalid_root);
    let pending = pending_quota(&meta, &wheel, wheel.len() as u64)
        .ok()
        .expect("the upload to reserve its exact capacity");

    // Staging failures must release pending quota reservations.
    let result = stage_publish(&blobs, prepared).await;
    drop(pending);

    assert!(matches!(result, Err(UploadStoreError::Blob(_))));
    assert_eq!(
        meta.quota_resource_usage("hosted", "flask").unwrap().artifact_bytes,
        peryx_storage::meta::QuotaValue::default()
    );
    assert!(meta.list_upload_entries("hosted", "flask").unwrap().is_empty());
}

#[tokio::test]
async fn test_store_prepared_quota_releases_when_the_existing_record_is_invalid() {
    let wheel = wheel_metadata("Flask", "1.0");
    let (_staged_dir, staged) = super::support::staged_upload(&wheel);
    let prepared = prepare(staged_form(&wheel), staged, "root/hosted", 1000).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    meta.put_upload("hosted", "flask", FILENAME, b"invalid-json").unwrap();
    let pending = pending_quota(&meta, &wheel, wheel.len() as u64)
        .ok()
        .expect("the upload to reserve its exact capacity");

    // Record failures after blob staging must roll back quota reservations.
    let staged = stage_publish(&blobs, prepared).await.unwrap();
    let result = commit_publish(&meta, "hosted", staged, Some(pending), true, None);

    assert!(matches!(result, Err(UploadStoreError::Parse(_))));
    assert_eq!(
        meta.quota_resource_usage("hosted", "flask").unwrap().artifact_bytes,
        peryx_storage::meta::QuotaValue::default()
    );
    assert_eq!(
        meta.list_upload_entries("hosted", "flask").unwrap(),
        vec![(FILENAME.to_owned(), b"invalid-json".to_vec())]
    );
}

#[rstest::rstest]
#[case::added(None, Some("YmFy"))]
#[case::removed(Some("YmFy"), None)]
#[case::changed(Some("YmFy"), Some("YmF6"))]
fn test_same_bytes_reupload_rejects_a_move_in_the_publications_attestations(
    #[case] first: Option<&str>,
    #[case] second: Option<&str>,
) {
    let wheel = wheel_metadata("Flask", "1.0");
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    assert!(publish_wheel(&meta, &blobs, &wheel, first).unwrap());

    let result = publish_wheel(&meta, &blobs, &wheel, second);

    assert!(
        matches!(&result, Err(UploadStoreError::ProvenanceMismatch(filename)) if filename == FILENAME),
        "{result:?}"
    );
}

#[test]
fn test_same_bytes_reupload_of_the_same_bundle_is_an_idempotent_no_op() {
    let wheel = wheel_metadata("Flask", "1.0");
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    assert!(publish_wheel(&meta, &blobs, &wheel, Some("YmFy")).unwrap());

    assert!(!publish_wheel(&meta, &blobs, &wheel, Some("YmFy")).unwrap());
}

/// An import commits the same verified bytes a push does, so the projection answers for its
/// distribution and its metadata sidecar alike. A digest the import never wrote keeps no row, which is
/// what separates an artifact this node does not hold from one it holds and never recorded.
#[test]
fn test_store_prepared_blocking_records_a_placement_for_every_blob_it_commits() {
    let wheel = wheel_metadata("Flask", "1.0");
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));

    publish_wheel(&meta, &blobs, &wheel, None).unwrap();

    let hosted = Some(peryx_ha::ArtifactPlacement::record(
        peryx_ha::ArtifactSource::Hosted,
        true,
    ));
    let artifact = Digest::of(&wheel);
    let metadata = meta.get_metadata_digest(artifact.as_str()).unwrap();
    assert_eq!(meta.get_artifact_placement(artifact.as_str()).unwrap(), hosted);
    assert_eq!(
        meta.get_artifact_placement(&metadata.expect("the import records a metadata sibling"))
            .unwrap(),
        hosted
    );
    assert_eq!(meta.get_artifact_placement(&"0".repeat(64)).unwrap(), None);
}

#[rstest::rstest]
#[case::metadata_sibling(false)]
#[case::archive_fallback(true)]
fn test_store_prepared_blocking_backfills_legacy_release_imports(#[case] archive_fallback: bool) {
    let directory = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(directory.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(directory.path().join("blobs"));
    let first_filename = "Flask-1.0-py3-none-any.whl";
    let first = wheel_metadata_bytes(
        b"Metadata-Version: 2.5\nName: Flask\nVersion: 1.0\nRequires-Python: >=3.8\nImport-Name: flask\n",
    );
    assert!(publish_named_wheel(&meta, &blobs, &first, first_filename).unwrap());
    let mut uploaded: crate::upload::Uploaded =
        serde_json::from_slice(&meta.get_upload("hosted", "flask", first_filename).unwrap().unwrap()).unwrap();
    uploaded.imports = None;
    if archive_fallback {
        uploaded.file.clear_metadata();
    }
    meta.put_upload(
        "hosted",
        "flask",
        first_filename,
        &serde_json::to_vec(&uploaded).unwrap(),
    )
    .unwrap();
    let second = wheel_metadata_bytes(
        b"Metadata-Version: 2.5\nName: Flask\nVersion: 1.0\nRequires-Python: >=3.8\nImport-Namespace: flask\n",
    );

    let result = publish_named_wheel(&meta, &blobs, &second, "flask-1.0-py3-none-any.whl");

    assert!(
        matches!(
            &result,
            Err(UploadStoreError::ReleaseImports(message)) if message.contains("exclusive and shared")
        ),
        "{result:?}"
    );
    let stored: crate::upload::Uploaded =
        serde_json::from_slice(&meta.get_upload("hosted", "flask", first_filename).unwrap().unwrap()).unwrap();
    assert!(stored.imports.is_some());
}

#[test]
fn test_store_prepared_blocking_reports_a_missing_legacy_archive_digest() {
    let directory = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(directory.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(directory.path().join("blobs"));
    let first_filename = "Flask-1.0-py3-none-any.whl";
    let first = wheel_metadata("Flask", "1.0");
    assert!(publish_named_wheel(&meta, &blobs, &first, first_filename).unwrap());
    let mut uploaded: crate::upload::Uploaded =
        serde_json::from_slice(&meta.get_upload("hosted", "flask", first_filename).unwrap().unwrap()).unwrap();
    uploaded.imports = None;
    uploaded.file.clear_metadata();
    uploaded.file.hashes.clear();
    meta.put_upload(
        "hosted",
        "flask",
        first_filename,
        &serde_json::to_vec(&uploaded).unwrap(),
    )
    .unwrap();

    let result = publish_named_wheel(
        &meta,
        &blobs,
        &wheel_metadata("Flask", "1.0"),
        "flask-1.0-py3-none-any.whl",
    );

    assert!(matches!(
        result,
        Err(UploadStoreError::MissingSha256(filename)) if filename == first_filename
    ));
}

/// Backfill reads only the live records of the release being published. A trashed record or one from
/// another release is left alone, so one that could not be classified blocks nothing.
#[rstest::rstest]
#[case::trashed("Flask-1.0-py3-none-any.whl", "1.0", true)]
#[case::other_release("Flask-2.0-py3-none-any.whl", "2.0", false)]
fn test_store_prepared_blocking_backfills_only_live_records_of_the_release(
    #[case] filename: &str,
    #[case] version: &str,
    #[case] trashed: bool,
) {
    let directory = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(directory.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(directory.path().join("blobs"));
    let first_filename = "Flask-1.0-py3-none-any.whl";
    assert!(publish_named_wheel(&meta, &blobs, &wheel_metadata("Flask", "1.0"), first_filename).unwrap());
    let mut uploaded = unclassify_wheel(&meta, first_filename);
    uploaded.file.clear_metadata();
    uploaded.file.hashes.clear();
    uploaded.version = version.to_owned();
    uploaded.trashed = trashed.then_some(peryx_core::TrashInfo {
        deleted_at_unix: 1000,
        actor: None,
        reason: None,
    });
    put_legacy_wheel(&meta, filename, &uploaded);

    let result = publish_named_wheel(
        &meta,
        &blobs,
        &wheel_metadata("Flask", "1.0"),
        "flask-1.0-py3-none-any.whl",
    );

    assert!(result.is_ok(), "{result:?}");
    let stored: crate::upload::Uploaded =
        serde_json::from_slice(&meta.get_upload("hosted", "flask", filename).unwrap().unwrap()).unwrap();
    assert_eq!(stored, uploaded);
}

#[test]
fn test_upload_store_error_maps_a_typed_store_failure() {
    let error =
        crate::store::UploadWriteError::Meta(peryx_storage::meta::MetaError::DriverPrecondition("store".to_owned()));
    assert!(matches!(UploadStoreError::from(error), UploadStoreError::Meta(_)));
}

#[rstest::rstest]
#[case::tar_gz("Flask-1.0.tar.gz")]
#[case::zip("Flask-1.0.zip")]
fn test_verified_archive_metadata_reads_sdists(#[case] filename: &str) {
    let bytes = sdist_with_license(filename, false);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(filename);
    std::fs::write(&path, &bytes).unwrap();

    let metadata = crate::upload::verified_archive_metadata(filename, &path, &Digest::of(&bytes))
        .unwrap()
        .unwrap();

    assert!(std::str::from_utf8(&metadata).unwrap().contains("Name: Flask"));
}

#[test]
fn test_verified_archive_metadata_rejects_a_digest_mismatch() {
    let bytes = wheel_metadata("Flask", "1.0");
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(FILENAME);
    std::fs::write(&path, &bytes).unwrap();

    let error = crate::upload::verified_archive_metadata(FILENAME, &path, &Digest::of(b"other")).unwrap_err();

    assert!(matches!(error, crate::upload::LegacyMetadataError::CorruptDigest));
}

#[test]
fn test_verified_archive_metadata_reports_open_read_and_filename_errors() {
    let directory = tempfile::tempdir().unwrap();
    let missing =
        crate::upload::verified_archive_metadata(FILENAME, &directory.path().join("missing"), &Digest::of(b"missing"))
            .unwrap_err();
    assert!(matches!(missing, crate::upload::LegacyMetadataError::Archive(_)));

    let read =
        crate::upload::verified_archive_metadata(FILENAME, directory.path(), &Digest::of(b"directory")).unwrap_err();
    assert!(matches!(read, crate::upload::LegacyMetadataError::Archive(_)));

    let path = directory.path().join("invalid");
    std::fs::write(&path, b"archive").unwrap();
    let filename = crate::upload::verified_archive_metadata("invalid", &path, &Digest::of(b"archive")).unwrap_err();
    assert!(matches!(filename, crate::upload::LegacyMetadataError::Archive(_)));
}

#[rstest::rstest]
#[case::utf8(b"\xff")]
#[case::syntax(b"not metadata")]
#[case::name(b"Metadata-Version: 2.5\nName: Other\nVersion: 1.0\n")]
#[case::version(b"Metadata-Version: 2.5\nName: Flask\nVersion: 2.0\n")]
fn test_store_prepared_blocking_rejects_invalid_legacy_metadata(#[case] metadata: &[u8]) {
    let directory = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(directory.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(directory.path().join("blobs"));
    assert!(publish_named_wheel(&meta, &blobs, &wheel_metadata("Flask", "1.0"), FILENAME).unwrap());
    let mut uploaded = unclassify_wheel(&meta, FILENAME);
    let digest = blobs.blocking().put_bytes(metadata).unwrap();
    uploaded.file.set_metadata(crate::CoreMetadata::Hashes(BTreeMap::from([(
        "sha256".to_owned(),
        digest.as_str().to_owned(),
    )])));
    put_legacy_wheel(&meta, FILENAME, &uploaded);

    let result = publish_named_wheel(
        &meta,
        &blobs,
        &wheel_metadata("Flask", "1.0"),
        "flask-1.0-py3-none-any.whl",
    );

    assert!(matches!(result, Err(UploadStoreError::Meta(_))), "{result:?}");
}

#[test]
fn test_store_prepared_blocking_reports_a_corrupt_release_import_projection() {
    let directory = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(directory.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(directory.path().join("blobs"));
    assert!(publish_named_wheel(&meta, &blobs, &wheel_metadata("Flask", "1.0"), FILENAME).unwrap());
    let uploaded = unclassify_wheel(&meta, FILENAME);
    put_legacy_wheel(&meta, FILENAME, &uploaded);
    meta.put_driver_value("pypi\0q\0hosted/flask/1", b"{").unwrap();

    let result = publish_named_wheel(
        &meta,
        &blobs,
        &wheel_metadata("Flask", "1.0"),
        "flask-1.0-py3-none-any.whl",
    );

    assert!(
        matches!(result, Err(UploadStoreError::Meta(ref error)) if error.to_string().contains("corrupt release import constraint")),
        "{result:?}"
    );
}

#[test]
fn test_store_prepared_blocking_rejects_a_metadata_sidecar_without_a_digest() {
    let directory = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(directory.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(directory.path().join("blobs"));
    assert!(publish_named_wheel(&meta, &blobs, &wheel_metadata("Flask", "1.0"), FILENAME).unwrap());
    let mut uploaded = unclassify_wheel(&meta, FILENAME);
    uploaded.file.set_metadata(crate::CoreMetadata::Hashes(BTreeMap::new()));
    put_legacy_wheel(&meta, FILENAME, &uploaded);

    let result = publish_named_wheel(
        &meta,
        &blobs,
        &wheel_metadata("Flask", "1.0"),
        "flask-1.0-py3-none-any.whl",
    );

    assert!(matches!(result, Err(UploadStoreError::Meta(_))), "{result:?}");
}

#[test]
fn test_store_prepared_blocking_rejects_a_corrupt_metadata_sidecar() {
    let directory = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(directory.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(directory.path().join("blobs"));
    assert!(publish_named_wheel(&meta, &blobs, &wheel_metadata("Flask", "1.0"), FILENAME).unwrap());
    let mut uploaded = unclassify_wheel(&meta, FILENAME);
    let digest = blobs.blocking().put_bytes(b"metadata").unwrap();
    let lease = blobs.blocking().materialize(&digest).unwrap();
    std::fs::write(lease.path(), b"changed").unwrap();
    drop(lease);
    uploaded.file.set_metadata(crate::CoreMetadata::Hashes(BTreeMap::from([(
        "sha256".to_owned(),
        digest.as_str().to_owned(),
    )])));
    put_legacy_wheel(&meta, FILENAME, &uploaded);

    let result = publish_named_wheel(
        &meta,
        &blobs,
        &wheel_metadata("Flask", "1.0"),
        "flask-1.0-py3-none-any.whl",
    );

    assert!(matches!(result, Err(UploadStoreError::Meta(_))), "{result:?}");
}

#[test]
fn test_store_prepared_blocking_rejects_a_corrupt_legacy_archive() {
    let directory = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(directory.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(directory.path().join("blobs"));
    assert!(publish_named_wheel(&meta, &blobs, &wheel_metadata("Flask", "1.0"), FILENAME).unwrap());
    let mut uploaded = unclassify_wheel(&meta, FILENAME);
    uploaded.file.clear_metadata();
    let digest = blobs.blocking().put_bytes(&wheel_metadata("Flask", "1.0")).unwrap();
    let lease = blobs.blocking().materialize(&digest).unwrap();
    std::fs::write(lease.path(), b"changed").unwrap();
    drop(lease);
    uploaded
        .file
        .hashes
        .insert("sha256".to_owned(), digest.as_str().to_owned());
    put_legacy_wheel(&meta, FILENAME, &uploaded);

    let result = publish_named_wheel(
        &meta,
        &blobs,
        &wheel_metadata("Flask", "1.0"),
        "flask-1.0-py3-none-any.whl",
    );

    assert!(matches!(result, Err(UploadStoreError::Meta(_))), "{result:?}");
}

#[rstest::rstest]
#[case::missing_metadata(wheel_without_metadata(), false)]
#[case::invalid_archive(b"not an archive".to_vec(), true)]
fn test_store_prepared_blocking_handles_archive_metadata_failures(
    #[case] archive: Vec<u8>,
    #[case] storage_failure: bool,
) {
    let directory = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(directory.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(directory.path().join("blobs"));
    assert!(publish_named_wheel(&meta, &blobs, &wheel_metadata("Flask", "1.0"), FILENAME).unwrap());
    let mut uploaded = unclassify_wheel(&meta, FILENAME);
    uploaded.file.clear_metadata();
    let digest = blobs.blocking().put_bytes(&archive).unwrap();
    uploaded
        .file
        .hashes
        .insert("sha256".to_owned(), digest.as_str().to_owned());
    put_legacy_wheel(&meta, FILENAME, &uploaded);

    let result = publish_named_wheel(
        &meta,
        &blobs,
        &wheel_metadata("Flask", "1.0"),
        "flask-1.0-py3-none-any.whl",
    );

    if storage_failure {
        assert!(matches!(result, Err(UploadStoreError::Meta(_))), "{result:?}");
    } else {
        assert!(matches!(result, Err(UploadStoreError::ReleaseImports(_))), "{result:?}");
    }
}
