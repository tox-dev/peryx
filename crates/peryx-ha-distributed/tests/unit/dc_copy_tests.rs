use std::collections::HashMap;
use std::num::{NonZeroU64, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use bytes::Bytes;
use peryx_storage::blob::{BlobErrorKind, BlobStore, Digest};

use crate::blob::{BlobRequest, BlobTransport, ByteRange, LoopbackBlobSource};
use crate::dc_copy::{CopyError, copy_blob_to_target};
use crate::peer::{TransferLimits, TransportError};

const CONTENT: &[u8] = b"artifact-bytes";
/// The metadata frame cap the production copier used to fetch whole blobs under.
const FRAME_CAP: usize = 4 * 1024 * 1024;
const STAGE_PREFIX: &[u8] = b".peryx-stage-";

fn limits(max_encoded_bytes: usize) -> TransferLimits {
    TransferLimits {
        max_operations: NonZeroUsize::new(256).unwrap(),
        max_encoded_bytes: NonZeroU64::new(u64::try_from(max_encoded_bytes).unwrap()).unwrap(),
    }
}

fn nz(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).unwrap()
}

fn source_of(digest: &Digest, content: Bytes, cap: usize) -> LoopbackBlobSource {
    LoopbackBlobSource::new(HashMap::from([(digest.clone(), content)]), limits(cap))
}

fn source_holding(digest: &Digest) -> LoopbackBlobSource {
    source_of(digest, Bytes::from_static(CONTENT), FRAME_CAP)
}

fn empty_source() -> LoopbackBlobSource {
    LoopbackBlobSource::new(HashMap::new(), limits(FRAME_CAP))
}

fn store_in(dir: &Path, name: &str) -> BlobStore {
    BlobStore::new(dir.join(name))
}

/// One byte past the cap: the smallest blob the whole-blob copy could not carry.
fn oversized() -> (Bytes, Digest) {
    let content = Bytes::from(vec![7u8; FRAME_CAP + 1]);
    let digest = Digest::of(&content);
    (content, digest)
}

fn staged_bytes(root: &Path) -> u64 {
    std::fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap())
        .filter(|entry| entry.file_name().as_encoded_bytes().starts_with(STAGE_PREFIX))
        .map(|entry| entry.metadata().unwrap().len())
        .sum()
}

/// Reports what the target stage already held when each range was asked for, so a test can pin both
/// the request plan and the fact that the bytes went to disk instead of into one buffer.
struct WatchingSource {
    inner: LoopbackBlobSource,
    root: PathBuf,
    observed: Mutex<Vec<(Option<ByteRange>, u64)>>,
}

impl WatchingSource {
    fn new(inner: LoopbackBlobSource, root: PathBuf) -> Self {
        Self {
            inner,
            root,
            observed: Mutex::new(Vec::new()),
        }
    }

    fn ranges(&self) -> Vec<Option<ByteRange>> {
        self.observed().into_iter().map(|(range, _)| range).collect()
    }

    fn staged_before_each_fetch(&self) -> Vec<u64> {
        self.observed().into_iter().map(|(_, staged)| staged).collect()
    }

    fn observed(&self) -> Vec<(Option<ByteRange>, u64)> {
        self.observed.lock().unwrap().clone()
    }
}

#[async_trait]
impl BlobTransport for WatchingSource {
    async fn fetch_blob(&self, request: BlobRequest) -> Result<Vec<u8>, TransportError> {
        self.observed
            .lock()
            .unwrap()
            .push((request.range, staged_bytes(&self.root)));
        self.inner.fetch_blob(request).await
    }
}

#[tokio::test]
async fn test_copy_publishes_the_verified_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let digest = Digest::of(CONTENT);
    let target = store_in(dir.path(), "dc-a");

    copy_blob_to_target(&source_holding(&digest), &target, &digest, CONTENT.len(), nz(64))
        .await
        .unwrap();

    assert_eq!(target.read(&digest).unwrap(), CONTENT);
}

#[tokio::test]
async fn test_a_whole_fetch_of_an_oversized_blob_still_exceeds_the_frame_cap() {
    let (content, digest) = oversized();
    let source = source_of(&digest, content, FRAME_CAP);

    let error = source
        .fetch_blob(BlobRequest { digest, range: None })
        .await
        .unwrap_err();

    assert_eq!(
        error,
        TransportError::FrameTooLarge {
            limit: u64::try_from(FRAME_CAP).unwrap(),
            actual: u64::try_from(FRAME_CAP).unwrap() + 1,
        }
    );
}

#[tokio::test]
async fn test_copy_transfers_a_blob_larger_than_the_frame_cap() {
    let dir = tempfile::tempdir().unwrap();
    let (content, digest) = oversized();
    let target = store_in(dir.path(), "dc-a");
    let source = source_of(&digest, content.clone(), FRAME_CAP);

    copy_blob_to_target(&source, &target, &digest, content.len(), nz(FRAME_CAP))
        .await
        .unwrap();

    assert_eq!(target.read(&digest).unwrap(), content);
}

#[tokio::test]
async fn test_copy_requests_one_range_per_chunk_and_never_the_whole_blob() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("dc-a");
    let digest = Digest::of(CONTENT);
    let target = BlobStore::new(&root);
    // A cap below the blob length rejects any whole-blob request outright.
    let source = WatchingSource::new(source_of(&digest, Bytes::from_static(CONTENT), 6), root);

    copy_blob_to_target(&source, &target, &digest, CONTENT.len(), nz(6))
        .await
        .unwrap();

    assert_eq!(
        source.ranges(),
        vec![
            Some(ByteRange { offset: 0, length: 6 }),
            Some(ByteRange { offset: 6, length: 6 }),
            Some(ByteRange { offset: 12, length: 2 }),
        ]
    );
}

#[tokio::test]
async fn test_copy_publishes_a_multi_range_blob_in_offset_order() {
    let dir = tempfile::tempdir().unwrap();
    let digest = Digest::of(CONTENT);
    let target = store_in(dir.path(), "dc-a");

    copy_blob_to_target(
        &source_of(&digest, Bytes::from_static(CONTENT), 6),
        &target,
        &digest,
        CONTENT.len(),
        nz(6),
    )
    .await
    .unwrap();

    assert_eq!(target.read(&digest).unwrap(), CONTENT);
}

#[tokio::test]
async fn test_copy_stages_each_range_instead_of_reassembling_the_blob() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("dc-a");
    let (content, digest) = oversized();
    let target = BlobStore::new(&root);
    let chunk = 2 * 1024 * 1024;
    let source = WatchingSource::new(source_of(&digest, content, FRAME_CAP), root);

    copy_blob_to_target(&source, &target, &digest, FRAME_CAP + 1, nz(chunk))
        .await
        .unwrap();

    let staged = u64::try_from(chunk).unwrap();
    assert_eq!(source.staged_before_each_fetch(), vec![0, staged, 2 * staged]);
}

#[tokio::test]
async fn test_copy_publishes_an_empty_blob_without_requesting_a_range() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("dc-a");
    let digest = Digest::of(b"");
    let target = BlobStore::new(&root);
    let source = WatchingSource::new(source_of(&digest, Bytes::new(), FRAME_CAP), root);

    copy_blob_to_target(&source, &target, &digest, 0, nz(64)).await.unwrap();

    assert_eq!(
        (target.read(&digest).unwrap(), source.ranges()),
        (Vec::new(), Vec::new())
    );
}

#[tokio::test]
async fn test_copy_reports_a_fetch_error_when_the_source_lacks_the_blob() {
    let dir = tempfile::tempdir().unwrap();
    let digest = Digest::of(CONTENT);
    let target = store_in(dir.path(), "dc-a");

    let error = copy_blob_to_target(&empty_source(), &target, &digest, CONTENT.len(), nz(64))
        .await
        .unwrap_err();

    assert_eq!(
        error.to_string(),
        format!(
            "copy source could not serve the blob: peer holds no blob for digest {}",
            digest.as_str()
        )
    );
}

#[tokio::test]
async fn test_copy_rejects_a_range_the_source_serves_at_the_wrong_length() {
    let dir = tempfile::tempdir().unwrap();
    let digest = Digest::of(CONTENT);
    let target = store_in(dir.path(), "dc-a");

    // A placement size past the bytes the source holds truncates the served range.
    let error = copy_blob_to_target(&source_holding(&digest), &target, &digest, CONTENT.len() + 3, nz(64))
        .await
        .unwrap_err();

    assert_eq!(
        error.to_string(),
        "copy source served 14 bytes for the 17-byte range at offset 0"
    );
}

#[tokio::test]
async fn test_a_failed_range_publishes_nothing_and_leaves_no_stage() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("dc-a");
    let digest = Digest::of(CONTENT);
    let target = BlobStore::new(&root);

    copy_blob_to_target(&source_holding(&digest), &target, &digest, CONTENT.len() + 3, nz(64))
        .await
        .unwrap_err();

    assert_eq!((target.exists(&digest), staged_bytes(&root)), (false, 0));
}

#[tokio::test]
async fn test_copy_rejects_bytes_that_do_not_hash_to_the_requested_digest() {
    let dir = tempfile::tempdir().unwrap();
    let digest = Digest::of(CONTENT);
    let target = store_in(dir.path(), "dc-a");
    let source = source_of(&digest, Bytes::from_static(b"substituted!!!"), FRAME_CAP);

    let error = copy_blob_to_target(&source, &target, &digest, CONTENT.len(), nz(64))
        .await
        .unwrap_err();

    assert!(matches!(
        &error,
        CopyError::Publish(publish) if publish.kind() == BlobErrorKind::DigestMismatch
    ));
}

#[tokio::test]
async fn test_copy_reports_a_publish_error_when_the_target_cannot_write() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("blocked");
    std::fs::write(&root, b"not a directory").unwrap();
    let digest = Digest::of(CONTENT);
    let target = BlobStore::new(&root);

    let error = copy_blob_to_target(&source_holding(&digest), &target, &digest, CONTENT.len(), nz(64))
        .await
        .unwrap_err();

    assert!(matches!(error, CopyError::Publish(_)));
    assert!(
        error
            .to_string()
            .starts_with("copy target could not publish the blob: ")
    );
}
