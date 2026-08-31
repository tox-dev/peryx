use futures_util::StreamExt as _;

use crate::blob::{
    BlobBackend, BlobErrorKind, BlobMetadata, BlobOperation, BlobRead, BlobReadBody, BlobScanError, BlobStorage,
    Digest, DurabilityCapabilities, PlacementReceipt, S3Config, S3Settings, WriteEvidence,
};

#[test]
fn test_filesystem_backend_id_matches_its_name() {
    let dir = tempfile::tempdir().unwrap();
    let storage = BlobStorage::filesystem(dir.path());
    assert_eq!(storage.backend_id().as_str(), storage.name());
    assert_eq!(storage.backend_id().as_str(), "filesystem");
}

fn dummy_s3(dir: &std::path::Path) -> BlobStorage {
    // S3 rejection must happen before any connection attempt.
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
    let s3 = dummy_s3(dir.path());
    assert!(s3.filesystem_store().is_none());
    assert_eq!(s3.name(), "s3");
}

#[tokio::test]
async fn test_backend_health_reports_an_invalid_filesystem_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("file");
    std::fs::write(&root, b"not a directory").unwrap();

    let error = BlobBackend::health(&BlobStorage::filesystem(root)).await.unwrap_err();

    assert_eq!(error.kind(), BlobErrorKind::Io);
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

    assert_eq!(
        receipt,
        PlacementReceipt {
            digest: digest.clone(),
            size: content.len() as u64,
            durability: DurabilityCapabilities::FILESYSTEM,
            evidence: WriteEvidence::NodeLocal,
        }
    );
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
    assert_eq!(storage.staged_upload_len("s").await.unwrap(), None);
    assert_eq!(
        storage
            .read_bytes(&Digest::of(b"streamed content"), u64::MAX)
            .await
            .unwrap(),
        b"streamed content"
    );

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

#[test]
fn test_blocking_storage_verifies_and_visits_filesystem_blobs() {
    let dir = tempfile::tempdir().unwrap();
    let storage = BlobStorage::filesystem(dir.path());
    let digest = storage.blocking().put_bytes(b"package").unwrap();
    assert!(storage.blocking().verify(&digest).unwrap());
    let mut entries = Vec::new();
    {
        let mut visit = |entry: crate::blob::BlobEntry| {
            entries.push((entry.digest, entry.bytes));
            Ok::<_, std::convert::Infallible>(())
        };
        storage.blocking().visit(&mut visit).unwrap();

        let missing = Digest::of(b"missing");
        let error = storage.blocking().verify(&missing).unwrap_err();
        assert_eq!(error.context().unwrap().operation, BlobOperation::Verify);
        assert!(matches!(
            storage.blocking().visit(|_| Err("stop")),
            Err(BlobScanError::Visit("stop"))
        ));
        let error = storage.blocking().materialize(&missing).unwrap_err();
        assert_eq!(error.context().unwrap().operation, BlobOperation::Materialize);

        let blocked = dir.path().join("blocked");
        std::fs::create_dir(&blocked).unwrap();
        std::fs::write(blocked.join("sha256"), b"file").unwrap();
        assert!(matches!(
            BlobStorage::filesystem(blocked).blocking().visit(&mut visit),
            Err(BlobScanError::Store(error)) if error.context().unwrap().operation == BlobOperation::List
        ));
    }
    assert_eq!(entries, vec![(Some(digest), 7)]);
}

#[test]
fn test_blocking_put_as_commits_when_the_digest_matches() {
    let dir = tempfile::tempdir().unwrap();
    let storage = BlobStorage::filesystem(dir.path());
    let digest = Digest::of(b"package");
    storage.blocking().put_bytes_as(b"package", &digest).unwrap();
    assert_eq!(storage.blocking().read_bytes(&digest, 7).unwrap(), b"package");
}

#[tokio::test]
async fn test_reversed_stream_reports_its_declared_range_when_polled_directly() {
    let digest = Digest::of(b"package");
    let read = BlobRead::new(
        "stream",
        digest,
        BlobMetadata {
            bytes: 7,
            modified: None,
        },
        std::ops::Range { start: 5, end: 1 },
        BlobReadBody::Stream(futures_util::stream::empty().boxed()),
    );
    assert_eq!(
        read.collect(7).await.unwrap_err().invalid_range_values(),
        Some((5, 1, 7))
    );
}

#[cfg(unix)]
#[tokio::test]
async fn test_filesystem_open_reports_non_missing_io_errors() {
    let dir = tempfile::tempdir().unwrap();
    let storage = BlobStorage::filesystem(dir.path());
    let digest = Digest::of(b"loop");
    let hex = digest.as_str();
    let path = dir.path().join(format!("sha256/{}/{}/{}", &hex[..2], &hex[2..4], hex));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&path, &path).unwrap();
    assert_eq!(
        storage.open(&digest, None).await.err().unwrap().kind(),
        BlobErrorKind::Io
    );
}

#[cfg(unix)]
#[tokio::test]
async fn test_filesystem_write_abort_reports_stage_removal_failure() {
    let dir = tempfile::tempdir().unwrap();
    let storage = BlobStorage::filesystem(dir.path());
    let write = storage.begin().await.unwrap();
    let path = std::fs::read_dir(dir.path()).unwrap().next().unwrap().unwrap().path();
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();

    let error = write.abort().await.unwrap_err();
    assert_eq!(error.context().unwrap().operation, BlobOperation::Write);
}

#[tokio::test]
async fn test_filesystem_staged_abort_reports_stage_removal_failure() {
    let dir = tempfile::tempdir().unwrap();
    let staged = BlobStorage::filesystem(dir.path())
        .stage_bytes(b"staged")
        .await
        .unwrap();
    let path = staged.with_materialized(std::path::Path::to_owned);
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();

    let error = staged.abort().await.unwrap_err();

    assert_eq!(error.context().unwrap().operation, BlobOperation::Write);
}

#[tokio::test]
async fn test_filesystem_write_starts_at_the_batch_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let mut write = BlobStorage::filesystem(dir.path()).begin().await.unwrap();
    let tail = write.tail().unwrap();
    write.write_chunk(bytes::Bytes::from(vec![0; 1_048_576])).await.unwrap();
    write.write_chunk(bytes::Bytes::from_static(b"x")).await.unwrap();

    assert_eq!(tail.open().unwrap().metadata().unwrap().len(), 1_048_576);

    write.abort().await.unwrap();
}
