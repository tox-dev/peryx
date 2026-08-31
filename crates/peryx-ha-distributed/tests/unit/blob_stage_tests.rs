use std::num::{NonZeroU64, NonZeroUsize};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use peryx_storage::blob::{BlobStorage, ChunkedDigest, Digest};
use tokio::sync::Semaphore;

use crate::blob::{BlobRequest, BlobTransport, ByteRange};
use crate::blob_pull::ChunkFailure;
use crate::blob_stage::{DEFAULT_RANGED_PULL_BUDGET, RangedPullBudget, StagedPullError, pull_blob_staged};
use crate::peer::TransportError;

#[derive(Debug, Default)]
struct Log {
    /// Every served range in the order its source began serving it.
    served: Vec<(usize, ByteRange)>,
    in_flight: usize,
    peak: usize,
}

enum Serves {
    Content(Bytes),
    Fail(TransportError),
    /// One byte short of the requested range.
    Short,
}

enum Gate {
    Open,
    /// Serves only once another source releases a permit.
    Wait(Arc<Semaphore>),
    /// Releases a permit after serving each range.
    Release(Arc<Semaphore>),
    /// Makes the blob root unwritable once the bytes are served, so publication fails after staging.
    #[cfg(unix)]
    Seal(std::path::PathBuf),
}

struct Peer {
    id: usize,
    serves: Serves,
    gate: Gate,
    log: Arc<Mutex<Log>>,
}

#[async_trait]
impl BlobTransport for Peer {
    async fn fetch_blob(&self, request: BlobRequest) -> Result<Vec<u8>, TransportError> {
        let range = request.range.expect("a staged pull always requests a range");
        if let Gate::Wait(gate) = &self.gate {
            gate.acquire().await.expect("the gate is never closed").forget();
        }
        {
            let mut log = self.log.lock().unwrap();
            log.served.push((self.id, range));
            log.in_flight += 1;
            log.peak = log.peak.max(log.in_flight);
        }
        // Every range yields once, so the scheduler must overlap them to reach a peak above one.
        tokio::task::yield_now().await;
        self.log.lock().unwrap().in_flight -= 1;
        if let Gate::Release(gate) = &self.gate {
            gate.add_permits(1);
        }
        #[cfg(unix)]
        if let Gate::Seal(root) = &self.gate {
            set_mode(root, 0o555);
        }
        match &self.serves {
            Serves::Content(content) => {
                let start = range.offset.min(content.len());
                let end = start.saturating_add(range.length).min(content.len());
                Ok(content.slice(start..end).to_vec())
            }
            Serves::Fail(error) => Err(error.clone()),
            Serves::Short => Ok(vec![0x00; range.length.saturating_sub(1)]),
        }
    }
}

fn log() -> Arc<Mutex<Log>> {
    Arc::new(Mutex::new(Log::default()))
}

fn peer(id: usize, content: &'static [u8], log: &Arc<Mutex<Log>>) -> Peer {
    Peer {
        id,
        serves: Serves::Content(Bytes::from_static(content)),
        gate: Gate::Open,
        log: Arc::clone(log),
    }
}

fn gated(id: usize, content: &'static [u8], gate: Gate, log: &Arc<Mutex<Log>>) -> Peer {
    Peer {
        id,
        serves: Serves::Content(Bytes::from_static(content)),
        gate,
        log: Arc::clone(log),
    }
}

fn broken(id: usize, serves: Serves, log: &Arc<Mutex<Log>>) -> Peer {
    Peer {
        id,
        serves,
        gate: Gate::Open,
        log: Arc::clone(log),
    }
}

fn budget(range_bytes: usize, max_in_flight: usize, max_resident_bytes: usize) -> RangedPullBudget {
    RangedPullBudget {
        range_bytes: NonZeroUsize::new(range_bytes).unwrap(),
        max_in_flight: NonZeroUsize::new(max_in_flight).unwrap(),
        max_resident_bytes: NonZeroUsize::new(max_resident_bytes).unwrap(),
    }
}

#[cfg(unix)]
fn set_mode(root: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::set_permissions(root, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn range(offset: usize, length: usize) -> ByteRange {
    ByteRange { offset, length }
}

fn stores() -> (tempfile::TempDir, BlobStorage) {
    let dir = tempfile::tempdir().unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    (dir, blobs)
}

fn catalog(content: &[u8], chunk: u64) -> ChunkedDigest {
    ChunkedDigest::of(content, NonZeroU64::new(chunk).unwrap())
}

fn served(log: &Arc<Mutex<Log>>) -> Vec<(usize, ByteRange)> {
    log.lock().unwrap().served.clone()
}

fn by_source(log: &Arc<Mutex<Log>>) -> Vec<(usize, ByteRange)> {
    let mut served = served(log);
    served.sort_by_key(|(id, range)| (*id, range.offset));
    served
}

const SIXTEEN: &[u8] = b"0123456789abcdef";
const TWENTY_FOUR: &[u8] = b"0123456789abcdefghijklmn";

#[tokio::test]
async fn test_pull_blob_staged_splits_the_ranges_between_two_healthy_sources() {
    let (_dir, blobs) = stores();
    let digest = Digest::of(SIXTEEN);
    let log = log();
    let sources = [peer(0, SIXTEEN, &log), peer(1, SIXTEEN, &log)];

    pull_blob_staged(&blobs, &[&sources[0], &sources[1]], &digest, 16, None, budget(4, 4, 64))
        .await
        .unwrap();

    assert_eq!(
        by_source(&log),
        vec![(0, range(0, 4)), (0, range(8, 4)), (1, range(4, 4)), (1, range(12, 4)),]
    );
}

#[tokio::test]
async fn test_pull_blob_staged_commits_the_pulled_bytes() {
    let (_dir, blobs) = stores();
    let digest = Digest::of(SIXTEEN);
    let log = log();
    let sources = [peer(0, SIXTEEN, &log), peer(1, SIXTEEN, &log)];

    let receipt = pull_blob_staged(&blobs, &[&sources[0], &sources[1]], &digest, 16, None, budget(4, 4, 64))
        .await
        .unwrap();

    assert_eq!((receipt.digest, receipt.size), (digest.clone(), 16));
    assert!(blobs.verify(&digest).await.unwrap());
}

#[tokio::test]
async fn test_pull_blob_staged_overlaps_up_to_the_in_flight_bound() {
    let (_dir, blobs) = stores();
    let digest = Digest::of(TWENTY_FOUR);
    let log = log();
    let source = peer(0, TWENTY_FOUR, &log);

    pull_blob_staged(&blobs, &[&source], &digest, 24, None, budget(4, 3, 1024))
        .await
        .unwrap();

    assert_eq!(log.lock().unwrap().peak, 3);
}

#[tokio::test]
async fn test_pull_blob_staged_holds_fewer_ranges_when_the_byte_budget_is_smaller() {
    let (_dir, blobs) = stores();
    let digest = Digest::of(TWENTY_FOUR);
    let log = log();
    let source = peer(0, TWENTY_FOUR, &log);

    pull_blob_staged(&blobs, &[&source], &digest, 24, None, budget(4, 8, 8))
        .await
        .unwrap();

    assert_eq!(log.lock().unwrap().peak, 2);
}

#[tokio::test]
async fn test_pull_blob_staged_transfers_a_range_wider_than_the_whole_byte_budget() {
    let (_dir, blobs) = stores();
    let digest = Digest::of(SIXTEEN);
    let log = log();
    let source = peer(0, SIXTEEN, &log);

    pull_blob_staged(&blobs, &[&source], &digest, 16, None, budget(4, 4, 1))
        .await
        .unwrap();

    assert_eq!(log.lock().unwrap().peak, 1);
    assert!(blobs.verify(&digest).await.unwrap());
}

#[tokio::test]
async fn test_pull_blob_staged_writes_in_offset_order_when_a_later_range_arrives_first() {
    let (_dir, blobs) = stores();
    let content = b"0123456789abcdef";
    let digest = Digest::of(content);
    let log = log();
    let gate = Arc::new(Semaphore::new(0));
    let first = gated(0, content, Gate::Wait(Arc::clone(&gate)), &log);
    let second = gated(1, content, Gate::Release(gate), &log);

    pull_blob_staged(&blobs, &[&first, &second], &digest, 16, None, budget(8, 2, 64))
        .await
        .unwrap();

    assert_eq!(served(&log), vec![(1, range(8, 8)), (0, range(0, 8))]);
    assert!(blobs.verify(&digest).await.unwrap());
}

#[tokio::test(start_paused = true)]
async fn test_pull_blob_staged_serves_other_ranges_while_one_source_is_blocked() {
    let (_dir, blobs) = stores();
    let digest = Digest::of(SIXTEEN);
    let log = log();
    let gate = Arc::new(Semaphore::new(0));
    let blocked = gated(0, SIXTEEN, Gate::Wait(Arc::clone(&gate)), &log);
    let healthy = gated(1, SIXTEEN, Gate::Release(gate), &log);

    let sources = [&blocked, &healthy];
    let pull = pull_blob_staged(&blobs, &sources, &digest, 16, None, budget(4, 4, 64));
    tokio::time::timeout(Duration::from_secs(30), pull)
        .await
        .expect("a blocked source must not stall the ranges assigned elsewhere")
        .unwrap();

    assert_eq!(served(&log).first(), Some(&(1, range(4, 4))));
}

#[tokio::test]
async fn test_pull_blob_staged_falls_through_a_source_that_serves_the_wrong_length() {
    let (_dir, blobs) = stores();
    let digest = Digest::of(SIXTEEN);
    let log = log();
    let short = broken(0, Serves::Short, &log);
    let healthy = peer(1, SIXTEEN, &log);

    pull_blob_staged(&blobs, &[&short, &healthy], &digest, 16, None, budget(16, 4, 64))
        .await
        .unwrap();

    assert_eq!(served(&log), vec![(0, range(0, 16)), (1, range(0, 16))]);
}

#[tokio::test]
async fn test_pull_blob_staged_reports_every_attempted_source_for_an_unserved_range() {
    let (_dir, blobs) = stores();
    let digest = Digest::of(SIXTEEN);
    let log = log();
    let down = broken(0, Serves::Fail(TransportError::Timeout), &log);
    let short = broken(1, Serves::Short, &log);

    let error = pull_blob_staged(&blobs, &[&down, &short], &digest, 16, None, budget(16, 4, 64))
        .await
        .unwrap_err();

    let failures = vec![
        (0, ChunkFailure::Transport(TransportError::Timeout)),
        (1, ChunkFailure::WrongLength { expected: 16, got: 15 }),
    ];
    assert!(matches!(&error, StagedPullError::RangeUnavailable(unavailable)
        if unavailable.index == 0 && unavailable.range == range(0, 16) && unavailable.failures == failures));
}

#[tokio::test]
async fn test_pull_blob_staged_publishes_nothing_when_a_range_is_unserved() {
    let (_dir, blobs) = stores();
    let digest = Digest::of(SIXTEEN);
    let log = log();
    let down = broken(0, Serves::Fail(TransportError::Timeout), &log);

    pull_blob_staged(&blobs, &[&down], &digest, 16, None, budget(16, 4, 64))
        .await
        .unwrap_err();

    assert!(blobs.head(&digest).await.unwrap().is_none());
}

#[tokio::test]
async fn test_pull_blob_staged_retries_a_digest_mismatch_under_a_rotated_assignment() {
    let (_dir, blobs) = stores();
    let digest = Digest::of(SIXTEEN);
    let log = log();
    let corrupt = peer(0, b"cccccccccccccccc", &log);
    let healthy = peer(1, SIXTEEN, &log);

    pull_blob_staged(&blobs, &[&corrupt, &healthy], &digest, 16, None, budget(16, 4, 64))
        .await
        .unwrap();

    assert_eq!(served(&log), vec![(0, range(0, 16)), (1, range(0, 16))]);
    assert!(blobs.verify(&digest).await.unwrap());
}

#[tokio::test]
async fn test_pull_blob_staged_exhausts_every_assignment_without_naming_a_source() {
    let (_dir, blobs) = stores();
    let digest = Digest::of(SIXTEEN);
    let log = log();
    let corrupt = peer(0, b"cccccccccccccccc", &log);
    let also_corrupt = peer(1, b"dddddddddddddddd", &log);

    let error = pull_blob_staged(&blobs, &[&corrupt, &also_corrupt], &digest, 16, None, budget(16, 4, 64))
        .await
        .unwrap_err();

    assert!(matches!(error, StagedPullError::DigestMismatch { attempts: 2 }));
}

#[tokio::test]
async fn test_pull_blob_staged_publishes_nothing_after_right_length_corruption() {
    let (_dir, blobs) = stores();
    let digest = Digest::of(SIXTEEN);
    let log = log();
    let corrupt = peer(0, b"cccccccccccccccc", &log);

    pull_blob_staged(&blobs, &[&corrupt], &digest, 16, None, budget(16, 4, 64))
        .await
        .unwrap_err();

    assert!(blobs.head(&digest).await.unwrap().is_none());
}

#[tokio::test]
async fn test_pull_blob_staged_takes_its_range_boundaries_from_a_trusted_catalog() {
    let (_dir, blobs) = stores();
    let digest = Digest::of(SIXTEEN);
    let log = log();
    let source = peer(0, SIXTEEN, &log);

    pull_blob_staged(
        &blobs,
        &[&source],
        &digest,
        16,
        Some(&catalog(SIXTEEN, 6)),
        budget(4, 4, 64),
    )
    .await
    .unwrap();

    assert_eq!(
        served(&log),
        vec![(0, range(0, 6)), (0, range(6, 6)), (0, range(12, 4))]
    );
}

#[tokio::test]
async fn test_pull_blob_staged_falls_through_a_source_failing_a_trusted_chunk_digest() {
    let (_dir, blobs) = stores();
    let digest = Digest::of(SIXTEEN);
    let log = log();
    let corrupt = peer(0, b"cccccccccccccccc", &log);
    let healthy = peer(1, SIXTEEN, &log);

    pull_blob_staged(
        &blobs,
        &[&corrupt, &healthy],
        &digest,
        16,
        Some(&catalog(SIXTEEN, 16)),
        budget(4, 4, 64),
    )
    .await
    .unwrap();

    assert_eq!(served(&log), vec![(0, range(0, 16)), (1, range(0, 16))]);
    assert!(blobs.verify(&digest).await.unwrap());
}

#[tokio::test]
async fn test_pull_blob_staged_names_each_source_that_fails_a_trusted_chunk_digest() {
    let (_dir, blobs) = stores();
    let digest = Digest::of(SIXTEEN);
    let log = log();
    let corrupt = peer(0, b"cccccccccccccccc", &log);
    let also_corrupt = peer(1, b"dddddddddddddddd", &log);

    let error = pull_blob_staged(
        &blobs,
        &[&corrupt, &also_corrupt],
        &digest,
        16,
        Some(&catalog(SIXTEEN, 16)),
        budget(4, 4, 64),
    )
    .await
    .unwrap_err();

    let failures = vec![(0, ChunkFailure::DigestMismatch), (1, ChunkFailure::DigestMismatch)];
    assert!(matches!(&error, StagedPullError::RangeUnavailable(unavailable)
        if unavailable.failures == failures));
}

#[tokio::test]
async fn test_pull_blob_staged_does_not_rotate_a_catalogued_pull_that_fails_the_whole_digest() {
    let (_dir, blobs) = stores();
    // The catalog matches the served bytes, so it disagrees with the digest the caller asked for.
    let digest = Digest::of(b"a different blob");
    let log = log();
    let first = peer(0, SIXTEEN, &log);
    let second = peer(1, SIXTEEN, &log);

    let error = pull_blob_staged(
        &blobs,
        &[&first, &second],
        &digest,
        16,
        Some(&catalog(SIXTEEN, 16)),
        budget(4, 4, 64),
    )
    .await
    .unwrap_err();

    assert!(matches!(error, StagedPullError::DigestMismatch { attempts: 1 }));
    assert_eq!(served(&log), vec![(0, range(0, 16))]);
}

#[tokio::test]
async fn test_pull_blob_staged_commits_an_empty_blob_without_any_range() {
    let (_dir, blobs) = stores();
    let digest = Digest::of(b"");
    let log = log();
    let source = peer(0, b"", &log);

    let receipt = pull_blob_staged(&blobs, &[&source], &digest, 0, None, DEFAULT_RANGED_PULL_BUDGET)
        .await
        .unwrap();

    assert_eq!((receipt.size, served(&log)), (0, Vec::new()));
}

#[cfg(unix)]
#[tokio::test]
async fn test_pull_blob_staged_reports_a_stage_the_local_store_refuses() {
    let (dir, blobs) = stores();
    let digest = Digest::of(SIXTEEN);
    let log = log();
    let source = peer(0, SIXTEEN, &log);
    let root = dir.path().join("blobs");
    std::fs::create_dir_all(&root).unwrap();
    set_mode(&root, 0o555);

    let error = pull_blob_staged(&blobs, &[&source], &digest, 16, None, budget(16, 4, 64))
        .await
        .unwrap_err();

    set_mode(&root, 0o755);
    assert!(matches!(error, StagedPullError::Stage(_)));
}

#[cfg(unix)]
#[tokio::test]
async fn test_pull_blob_staged_reports_a_publication_the_local_store_refuses() {
    let (dir, blobs) = stores();
    let digest = Digest::of(SIXTEEN);
    let log = log();
    let root = dir.path().join("blobs");
    std::fs::create_dir_all(&root).unwrap();
    let source = gated(0, SIXTEEN, Gate::Seal(root.clone()), &log);

    let error = pull_blob_staged(&blobs, &[&source], &digest, 16, None, budget(16, 4, 64))
        .await
        .unwrap_err();

    set_mode(&root, 0o755);
    assert!(matches!(error, StagedPullError::Stage(_)));
}
