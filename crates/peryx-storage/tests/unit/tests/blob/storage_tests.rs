use crate::blob::{BlobErrorKind, BlobStorage, Digest, DurabilityCapabilities, S3Config, S3Settings};

#[test]
fn test_filesystem_backend_id_matches_its_name() {
    let dir = tempfile::tempdir().unwrap();
    let storage = BlobStorage::filesystem(dir.path());
    assert_eq!(storage.backend_id().as_str(), storage.name());
    assert_eq!(storage.backend_id().as_str(), "filesystem");
}

fn dummy_s3(dir: &std::path::Path) -> BlobStorage {
    // A config with a valid-but-unreachable endpoint: the resumable-upload methods reject the object
    // store before any request leaves the process, so no live endpoint is contacted.
    let settings = S3Settings {
        endpoint: "http://127.0.0.1:1".to_owned(),
        bucket: "bucket".to_owned(),
        prefix: "cache".to_owned(),
        region: "us-east-1".to_owned(),
        path_style: true,
        request_timeout: std::time::Duration::from_secs(5),
        max_retries: 0,
        multipart_threshold: 5 << 20,
        part_size: 5 << 20,
        upload_concurrency: 2,
        conditional_writes: true,
        checksum_writes: true,
    };
    BlobStorage::s3(S3Config::new(settings).unwrap(), dir.join("staging"))
}

#[test]
fn test_filesystem_backend_exposes_its_store_and_s3_does_not() {
    let dir = tempfile::tempdir().unwrap();
    assert!(BlobStorage::filesystem(dir.path()).filesystem_store().is_some());
    assert!(dummy_s3(dir.path()).filesystem_store().is_none());
}

#[tokio::test]
async fn test_filesystem_commit_returns_a_durable_receipt_and_serves_the_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let storage = BlobStorage::filesystem(dir.path().join("blobs"));
    let content = b"durable replicated content";
    let digest = Digest::of(content);

    let mut write = storage.begin().await.unwrap();
    write.write_chunk(bytes::Bytes::from_static(content)).await.unwrap();
    let receipt = write.commit(&digest).await.unwrap();

    // The receipt proves the commit reached the filesystem durability boundary for these exact bytes.
    assert_eq!(receipt.digest, digest);
    assert_eq!(receipt.size, content.len() as u64);
    assert_eq!(receipt.durability, DurabilityCapabilities::FILESYSTEM);
    // The published bytes are served.
    assert_eq!(storage.read_bytes(&digest, u64::MAX).await.unwrap(), content);
}

#[tokio::test]
async fn test_a_digest_mismatch_yields_no_receipt_and_serves_no_partial_file() {
    let dir = tempfile::tempdir().unwrap();
    let storage = BlobStorage::filesystem(dir.path().join("blobs"));
    let expected = Digest::of(b"the promised bytes");

    let mut write = storage.begin().await.unwrap();
    write
        .write_chunk(bytes::Bytes::from_static(b"different bytes"))
        .await
        .unwrap();
    let error = write.commit(&expected).await.unwrap_err();

    // Corruption produces no receipt, and the unverified bytes never enter a served path.
    assert_eq!(error.kind(), BlobErrorKind::DigestMismatch);
    assert_eq!(
        storage.read_bytes(&expected, u64::MAX).await.unwrap_err().kind(),
        BlobErrorKind::NotFound
    );
}

#[tokio::test]
async fn test_filesystem_storage_stages_resumes_and_finishes_an_upload() {
    let dir = tempfile::tempdir().unwrap();
    let storage = BlobStorage::filesystem(dir.path().join("blobs"));

    assert_eq!(storage.stage_upload_chunk("s", 0, b"streamed ").await.unwrap(), 9);
    assert_eq!(storage.stage_upload_chunk("s", 9, b"content").await.unwrap(), 16);
    assert_eq!(storage.staged_upload_len("s").await.unwrap(), Some(16));

    storage
        .finish_upload("s", &Digest::of(b"streamed content"))
        .await
        .unwrap();
    // The blob is published and the stage cleared.
    assert_eq!(storage.staged_upload_len("s").await.unwrap(), None);

    // A discard of an already-absent stage is a no-op.
    storage.discard_upload("s").await.unwrap();
}

#[tokio::test]
async fn test_filesystem_storage_discards_a_stage() {
    let dir = tempfile::tempdir().unwrap();
    let storage = BlobStorage::filesystem(dir.path().join("blobs"));
    storage.stage_upload_chunk("s", 0, b"partial").await.unwrap();

    storage.discard_upload("s").await.unwrap();

    assert_eq!(storage.staged_upload_len("s").await.unwrap(), None);
}

#[tokio::test]
async fn test_object_store_rejects_resumable_upload_staging() {
    let dir = tempfile::tempdir().unwrap();
    let storage = dummy_s3(dir.path());

    // The object store proves durability as a service, so it does not offer a resumable local stage.
    assert_eq!(
        storage.stage_upload_chunk("s", 0, b"x").await.unwrap_err().kind(),
        BlobErrorKind::Unsupported
    );
    assert_eq!(
        storage.staged_upload_len("s").await.unwrap_err().kind(),
        BlobErrorKind::Unsupported
    );
    assert_eq!(
        storage.finish_upload("s", &Digest::of(b"x")).await.unwrap_err().kind(),
        BlobErrorKind::Unsupported
    );
    assert_eq!(
        storage.discard_upload("s").await.unwrap_err().kind(),
        BlobErrorKind::Unsupported
    );
}
