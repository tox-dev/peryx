use std::sync::Arc;

use super::{BlobError, S3Backend, S3Config, S3Error, S3Settings, UploadAcquisition};
use crate::blob::BlobErrorKind;
use tokio::sync::watch;

#[test]
fn test_blob_error_from_s3_error() {
    assert_eq!(BlobError::from(S3Error::NotFound).kind(), BlobErrorKind::Io);
    assert_eq!(
        BlobError::from(S3Error::Request("reset".to_owned())).kind(),
        BlobErrorKind::Io
    );
}

#[tokio::test]
async fn test_upload_acquisition_reuses_an_inflight_result() {
    let directory = tempfile::tempdir().unwrap();
    let backend = S3Backend::new(
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
        directory.path().to_owned(),
    );
    let journal = directory.path().join("journal");
    let (_, result) = watch::channel(Some(Ok("upload-1".to_owned())));
    backend
        .acquisitions
        .lock()
        .await
        .insert(journal.clone(), Arc::new(UploadAcquisition { result }));

    assert_eq!(backend.acquire_upload("key", &journal).await.unwrap(), "upload-1");
}
