use std::collections::HashMap;
use std::num::{NonZeroU64, NonZeroUsize};

use bytes::Bytes;
use peryx_storage::blob::{BlobStore, Digest};

use crate::blob::LoopbackBlobSource;
use crate::dc_copy::{CopyError, copy_blob_to_target};
use crate::peer::TransferLimits;

const CONTENT: &[u8] = b"artifact-bytes";

fn limits() -> TransferLimits {
    TransferLimits {
        max_operations: NonZeroUsize::new(256).unwrap(),
        max_encoded_bytes: NonZeroU64::new(4 * 1024 * 1024).unwrap(),
    }
}

fn source_holding(digest: &Digest) -> LoopbackBlobSource {
    LoopbackBlobSource::new(HashMap::from([(digest.clone(), Bytes::from_static(CONTENT))]), limits())
}

fn empty_source() -> LoopbackBlobSource {
    LoopbackBlobSource::new(HashMap::new(), limits())
}

fn store_in(dir: &std::path::Path, name: &str) -> BlobStore {
    BlobStore::new(dir.join(name))
}

#[tokio::test]
async fn test_copy_publishes_the_verified_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let digest = Digest::of(CONTENT);
    let target = store_in(dir.path(), "dc-a");

    copy_blob_to_target(&source_holding(&digest), &target, &digest)
        .await
        .unwrap();

    assert_eq!(target.read(&digest).unwrap(), CONTENT);
}

#[tokio::test]
async fn test_copy_reports_a_fetch_error_when_the_source_lacks_the_blob() {
    let dir = tempfile::tempdir().unwrap();
    let digest = Digest::of(CONTENT);
    let target = store_in(dir.path(), "dc-a");

    let error = copy_blob_to_target(&empty_source(), &target, &digest)
        .await
        .unwrap_err();

    assert!(matches!(error, CopyError::Fetch(_)), "{error}");
    assert!(error.to_string().contains("source could not serve"), "{error}");
}

#[tokio::test]
async fn test_copy_reports_a_publish_error_when_the_target_cannot_write() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("blocked");
    std::fs::write(&root, b"not a directory").unwrap();
    let digest = Digest::of(CONTENT);
    let target = BlobStore::new(&root);

    let error = copy_blob_to_target(&source_holding(&digest), &target, &digest)
        .await
        .unwrap_err();

    assert!(matches!(error, CopyError::Publish(_)), "{error}");
    assert!(error.to_string().contains("target could not publish"), "{error}");
}
