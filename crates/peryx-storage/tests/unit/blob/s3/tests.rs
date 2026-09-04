use std::path::Path;
use std::sync::Arc;

use super::{
    BlobError, Digest, MAX_MULTIPART_BYTES, MAX_PART_SIZE, S3Backend, S3Config, S3Error, S3Settings, UploadAcquisition,
    multipart_part_size,
};
use crate::blob::BlobErrorKind;
use rstest::rstest;
use tokio::sync::watch;

fn backend(staging: &Path) -> S3Backend {
    S3Backend::new(
        S3Config::new(S3Settings {
            endpoint: "http://127.0.0.1:1".to_owned(),
            bucket: "bucket".to_owned(),
            prefix: String::new(),
            region: "us-east-1".to_owned(),
            path_style: true,
            request_timeout: std::time::Duration::from_secs(1),
            max_retries: 0,
            multipart_threshold: 5 << 20,
            part_size: 5 << 20,
            upload_concurrency: 1,
            conditional_writes: true,
            checksum_writes: true,
        })
        .unwrap(),
        staging.to_owned(),
    )
}

#[test]
fn test_blob_error_from_s3_error() {
    assert_eq!(BlobError::from(S3Error::NotFound).kind(), BlobErrorKind::Io);
    assert_eq!(
        BlobError::from(S3Error::Request("reset".to_owned())).kind(),
        BlobErrorKind::Io
    );
}

#[rstest]
#[case::configured(5 << 20, 50_000 << 20, 5 << 20)]
#[case::rounded(5 << 20, (50_000 << 20) + 1, (5 << 20) + 1)]
#[case::protocol_max(5 << 20, MAX_MULTIPART_BYTES, MAX_PART_SIZE)]
fn test_multipart_part_size_stays_within_protocol_bounds(
    #[case] configured: u64,
    #[case] len: u64,
    #[case] expected: u64,
) {
    assert_eq!(multipart_part_size(configured, len).unwrap(), expected);
}

#[test]
fn test_multipart_part_size_rejects_the_first_byte_above_the_protocol_limit() {
    let len = MAX_MULTIPART_BYTES + 1;
    let error = multipart_part_size(5 << 20, len).unwrap_err();
    assert_eq!(error.kind(), BlobErrorKind::LimitExceeded);
    assert_eq!(
        error.to_string(),
        format!("blob size {len} exceeds {MAX_MULTIPART_BYTES} byte limit")
    );
}

/// The size check runs before the first request, so an object past the protocol limit is refused
/// against a bucket that was never contacted. The refusal still has to name the backend and the
/// operation, or an operator sees a bare size complaint with nothing to attach it to.
#[tokio::test]
async fn test_upload_stage_names_the_backend_that_refused_an_oversized_object() {
    let directory = tempfile::tempdir().unwrap();
    let backend = backend(directory.path());
    let digest = Digest::of(b"payload");
    let staged = directory.path().join("staged");
    std::fs::write(&staged, b"payload").unwrap();
    let len = MAX_MULTIPART_BYTES + 1;

    let error = backend.upload_stage(&digest, len, &staged).await.unwrap_err();

    assert_eq!(error.kind(), BlobErrorKind::LimitExceeded);
    assert_eq!(
        error.to_string(),
        format!(
            "s3 blob backend commit for {}: blob size {len} exceeds {MAX_MULTIPART_BYTES} byte limit",
            digest.as_str()
        )
    );
}

#[tokio::test]
async fn test_upload_acquisition_reuses_an_inflight_result() {
    let directory = tempfile::tempdir().unwrap();
    let backend = backend(directory.path());
    let journal = directory.path().join("journal");
    let (_, result) = watch::channel(Some(Ok("upload-1".to_owned())));
    backend
        .acquisitions
        .lock()
        .await
        .insert(journal.clone(), Arc::new(UploadAcquisition { result }));

    assert_eq!(backend.acquire_upload("key", &journal).await.unwrap(), "upload-1");
}

/// A malformed journal is removed as soon as recovery reads it, so its survival shows recovery left the
/// journal alone rather than that the abort failed.
#[tokio::test]
async fn test_recovery_leaves_a_journal_every_owner_has_not_released() {
    let directory = tempfile::tempdir().unwrap();
    let backend = backend(directory.path());
    let journal = backend.multipart_journal(&Digest::of(b"package"));
    std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
    std::fs::write(&journal, []).unwrap();
    let first = backend.staging().own(journal.clone());
    let second = backend.staging().own(journal.clone());

    drop(second);
    assert_eq!(backend.recover_multipart_uploads().await.unwrap(), 0);
    assert!(journal.exists());

    drop(first);
    assert_eq!(backend.recover_multipart_uploads().await.unwrap(), 0);
    assert!(!journal.exists());
}
