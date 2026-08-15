use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use blake2::Blake2bVar;
use blake2::digest::{Update as _, VariableOutput as _};
use peryx_storage::blob::{BlobStorage, Digest};
use peryx_storage::meta::MetaStore;
use serde_json::{Value, json};

use super::support::{hex, staged_form, wheel_metadata};
use crate::PackageName;
use crate::quota::{Admission, PendingQuota, admit_upload, quota_reservation};
use crate::store::PypiStore as _;
use crate::upload::{StagedUpload, UploadStoreError, commit_publish, prepare, stage_publish, store_prepared_blocking};

const FILENAME: &str = "Flask-1.0-py3-none-any.whl";

fn attestations_field(filename: &str, sha256: &str) -> String {
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
        "envelope": {"statement": statement, "signature": "YmFy"},
    }])
    .to_string()
}

fn blake2_256(bytes: &[u8]) -> String {
    let mut blake2 = Blake2bVar::new(32).unwrap();
    blake2.update(bytes);
    let mut digest = [0; 32];
    blake2.finalize_variable(&mut digest).unwrap();
    hex(&digest)
}

fn pending_quota(meta: &MetaStore, wheel: &[u8], limit: u64) -> Result<PendingQuota, u64> {
    let project = PackageName::new("Flask");
    let digest = Digest::of(wheel);
    let request = quota_reservation(
        "hosted",
        &project,
        Some("1.0"),
        digest.as_str(),
        wheel.len() as u64,
        peryx_storage::meta::AccountingClass::Hosted,
        1000,
    );
    match admit_upload(meta, request, limit, false).unwrap() {
        Admission::Reserved(pending) => Ok(pending),
        Admission::Rejected { total } => Err(total),
    }
}

#[test]
fn test_pending_quota_reports_the_projected_total() {
    let wheel = wheel_metadata("Flask", "1.0");
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();

    assert_eq!(pending_quota(&meta, &wheel, 0).err(), Some(wheel.len() as u64));
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

    let prepared = prepare(form, staged, "root/hosted", 1000).unwrap();
    assert!(
        prepared.provenance.is_some(),
        "attestations produce a provenance object"
    );

    let stored = store_prepared_blocking(&meta, &blobs, "hosted", prepared).unwrap();

    assert!(stored);
    let (provenance_sha, size) = meta
        .get_provenance(&sha)
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
    let published = commit_publish(&meta, "hosted", publish, None, true).unwrap();

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
    let prepared = prepare(form, staged, "root/hosted", 1000).unwrap();
    assert!(
        prepared.provenance.is_some(),
        "attestations produce a provenance object"
    );
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));

    let publish = stage_publish(&blobs, prepared).await.unwrap();
    let published = commit_publish(&meta, "hosted", publish, None, true).unwrap();

    assert_eq!(
        published.placements.len(),
        3,
        "an attested publish also places the provenance blob alongside the content and metadata",
    );
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
    let pending = pending_quota(&meta, &wheel, wheel.len() as u64).expect("the upload to reserve its exact capacity");

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
    let pending = pending_quota(&meta, &wheel, wheel.len() as u64).expect("the upload to reserve its exact capacity");

    // Record failures after blob staging must roll back quota reservations.
    let staged = stage_publish(&blobs, prepared).await.unwrap();
    let result = commit_publish(&meta, "hosted", staged, Some(pending), true);

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
