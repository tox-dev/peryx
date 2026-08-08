use bytes::Bytes;

use super::super::s3::{S3Backend, S3Config, S3Settings};
use super::super::{BlobBackend, BlobErrorKind, BlobStaged, Digest};

fn backend(staging: &std::path::Path) -> S3Backend {
    let settings = S3Settings {
        endpoint: "https://s3.example.com".to_owned(),
        bucket: "bucket".to_owned(),
        prefix: String::new(),
        region: "us-east-1".to_owned(),
        path_style: true,
        request_timeout: std::time::Duration::from_secs(5),
        max_retries: 0,
        multipart_threshold: 16 << 20,
        part_size: 8 << 20,
        upload_concurrency: 1,
        conditional_writes: true,
        checksum_writes: true,
    };
    S3Backend::new(S3Config::new(settings).unwrap(), staging.to_path_buf())
}

async fn staged(backend: &S3Backend) -> BlobStaged {
    let mut write = backend.begin().await.unwrap();
    write.write_chunk(Bytes::from_static(b"local")).await.unwrap();
    write.finish().await.unwrap()
}

#[tokio::test]
async fn test_s3_staged_rejects_a_blocking_commit() {
    // Staging is local, so no S3 request is made; the blocking facade is unsupported for S3.
    let dir = tempfile::tempdir().unwrap();
    let backend = backend(dir.path());
    let error = staged(&backend).await.commit_blocking().unwrap_err();
    assert_eq!(error.kind(), BlobErrorKind::Unsupported);
}

#[tokio::test]
async fn test_s3_staged_blocking_abort_drops_the_local_stage() {
    let dir = tempfile::tempdir().unwrap();
    let backend = backend(dir.path());
    staged(&backend).await.abort_blocking().unwrap();
}

#[tokio::test]
async fn test_s3_staged_rejects_a_digest_mismatch_without_uploading() {
    let dir = tempfile::tempdir().unwrap();
    let backend = backend(dir.path());
    let staged = staged(&backend).await;
    let path = staged.with_materialized(std::path::Path::to_owned);
    let error = staged.commit_as(&Digest::of(b"other")).await.unwrap_err();

    assert_eq!(error.kind(), BlobErrorKind::DigestMismatch);
    assert_eq!(error.context().unwrap().backend, "s3");
    assert!(!path.exists());
}
