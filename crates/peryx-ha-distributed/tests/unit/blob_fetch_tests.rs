use std::collections::HashMap;
use std::num::{NonZeroU64, NonZeroUsize};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use peryx_storage::blob::Digest;

use crate::blob::{BlobRequest, BlobTransport, LoopbackBlobSource};
use crate::blob_fetch::{FetchOutcome, fetch_missing};
use crate::peer::{TransferLimits, TransportError};

fn limits(max_bytes: u64) -> TransferLimits {
    TransferLimits {
        max_operations: NonZeroUsize::new(256).unwrap(),
        max_encoded_bytes: NonZeroU64::new(max_bytes).unwrap(),
    }
}

fn nz(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}

#[tokio::test]
async fn test_fetch_missing_reports_complete_when_every_blob_is_present() {
    let alpha = Digest::of(b"alpha");
    let beta = Digest::of(b"beta");
    let source = LoopbackBlobSource::new(
        HashMap::from([
            (alpha.clone(), Bytes::from_static(b"alpha")),
            (beta.clone(), Bytes::from_static(b"beta")),
        ]),
        limits(1024),
    );

    let report = fetch_missing(&source, &[alpha.clone(), beta.clone()], nz(2)).await;

    assert_eq!(report.outcome, FetchOutcome::Complete);
    assert_eq!(
        report.fetched,
        vec![(alpha, b"alpha".to_vec()), (beta, b"beta".to_vec())]
    );
}

#[tokio::test]
async fn test_fetch_missing_on_an_empty_set_is_complete() {
    let source = LoopbackBlobSource::new(HashMap::new(), limits(1024));

    let report = fetch_missing(&source, &[], nz(4)).await;

    assert_eq!(report.outcome, FetchOutcome::Complete);
    assert!(report.fetched.is_empty());
}

#[tokio::test]
async fn test_fetch_missing_fails_closed_on_a_digest_mismatch() {
    let claimed = Digest::of(b"the real blob");
    let source = LoopbackBlobSource::new(
        HashMap::from([(claimed.clone(), Bytes::from_static(b"substituted"))]),
        limits(1024),
    );

    let report = fetch_missing(&source, std::slice::from_ref(&claimed), nz(2)).await;

    assert_eq!(
        report.outcome,
        FetchOutcome::Failed {
            reason: "digest_mismatch",
            digest: claimed,
        }
    );
    assert!(report.fetched.is_empty());
}

#[tokio::test]
async fn test_fetch_missing_fails_closed_on_a_missing_blob() {
    let present = Digest::of(b"present");
    let absent = Digest::of(b"absent");
    let source = LoopbackBlobSource::new(
        HashMap::from([(present.clone(), Bytes::from_static(b"present"))]),
        limits(1024),
    );

    let report = fetch_missing(&source, &[present.clone(), absent.clone()], nz(2)).await;

    assert_eq!(
        report.outcome,
        FetchOutcome::Failed {
            reason: "blob_not_found",
            digest: absent,
        }
    );
    assert_eq!(report.fetched, vec![(present, b"present".to_vec())]);
}

enum Reply {
    Bytes(&'static [u8]),
    Capacity,
    Mismatch,
}

struct Scripted(HashMap<Digest, Reply>);

#[async_trait]
impl BlobTransport for Scripted {
    async fn fetch_blob(&self, request: BlobRequest) -> Result<Vec<u8>, TransportError> {
        match self.0.get(&request.digest) {
            Some(Reply::Bytes(bytes)) => Ok(bytes.to_vec()),
            Some(Reply::Capacity) => Err(TransportError::AtCapacity),
            Some(Reply::Mismatch) => Err(TransportError::DigestMismatch {
                expected: request.digest.as_str().to_owned(),
                actual: Digest::of(b"other").as_str().to_owned(),
            }),
            None => Err(TransportError::BlobNotFound {
                digest: request.digest.as_str().to_owned(),
            }),
        }
    }
}

#[tokio::test]
async fn test_fetch_missing_backpressures_on_a_capacity_cap() {
    let first = Digest::of(b"first");
    let second = Digest::of(b"second");
    let transport = Scripted(HashMap::from([
        (first.clone(), Reply::Capacity),
        (second.clone(), Reply::Capacity),
    ]));

    let report = fetch_missing(&transport, &[first, second], nz(2)).await;

    assert_eq!(report.outcome, FetchOutcome::Backpressured { pending: 2 });
    assert!(report.fetched.is_empty());
}

#[tokio::test]
async fn test_fetch_missing_keeps_the_blobs_a_backpressured_pass_still_fetched() {
    let done = Digest::of(b"done");
    let capped = Digest::of(b"capped");
    let transport = Scripted(HashMap::from([
        (done.clone(), Reply::Bytes(b"done")),
        (capped.clone(), Reply::Capacity),
    ]));

    let report = fetch_missing(&transport, &[done.clone(), capped], nz(2)).await;

    assert_eq!(report.outcome, FetchOutcome::Backpressured { pending: 1 });
    assert_eq!(report.fetched, vec![(done, b"done".to_vec())]);
}

#[tokio::test]
async fn test_fetch_missing_lets_a_terminal_failure_win_over_backpressure() {
    let mismatch = Digest::of(b"mismatch");
    let capped = Digest::of(b"capped");
    let transport = Scripted(HashMap::from([
        (mismatch.clone(), Reply::Mismatch),
        (capped.clone(), Reply::Capacity),
    ]));

    let report = fetch_missing(&transport, &[mismatch.clone(), capped], nz(2)).await;

    assert_eq!(
        report.outcome,
        FetchOutcome::Failed {
            reason: "digest_mismatch",
            digest: mismatch,
        }
    );
    assert!(report.fetched.is_empty());
}

#[tokio::test]
async fn test_fetch_missing_reports_the_first_terminal_in_request_order() {
    let first = Digest::of(b"first-bad");
    let second = Digest::of(b"second-bad");
    let transport = Scripted(HashMap::from([
        (first.clone(), Reply::Mismatch),
        (second.clone(), Reply::Mismatch),
    ]));

    let report = fetch_missing(&transport, &[first.clone(), second], nz(2)).await;

    assert_eq!(
        report.outcome,
        FetchOutcome::Failed {
            reason: "digest_mismatch",
            digest: first,
        }
    );
}

struct SlowFirstTerminal {
    slow: Digest,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl BlobTransport for SlowFirstTerminal {
    async fn fetch_blob(&self, request: BlobRequest) -> Result<Vec<u8>, TransportError> {
        if request.digest == self.slow {
            self.release.notified().await;
        } else {
            self.release.notify_one();
        }
        Err(TransportError::BlobNotFound {
            digest: request.digest.as_str().to_owned(),
        })
    }
}

#[tokio::test]
async fn test_fetch_missing_names_the_first_terminal_even_when_it_completes_last() {
    let slow = Digest::of(b"index-zero");
    let fast = Digest::of(b"index-one");
    let transport = SlowFirstTerminal {
        slow: slow.clone(),
        release: Arc::new(tokio::sync::Notify::new()),
    };

    let report = fetch_missing(&transport, &[slow.clone(), fast], nz(2)).await;

    assert_eq!(
        report.outcome,
        FetchOutcome::Failed {
            reason: "blob_not_found",
            digest: slow,
        }
    );
}

struct Counting {
    in_flight: Arc<AtomicUsize>,
    max: Arc<AtomicUsize>,
}

#[async_trait]
impl BlobTransport for Counting {
    async fn fetch_blob(&self, request: BlobRequest) -> Result<Vec<u8>, TransportError> {
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max.fetch_max(now, Ordering::SeqCst);
        tokio::task::yield_now().await;
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        Ok(request.digest.as_str().as_bytes().to_vec())
    }
}

fn digests(count: usize) -> Vec<Digest> {
    (0..count).map(|n| Digest::of(format!("blob-{n}").as_bytes())).collect()
}

#[tokio::test]
async fn test_fetch_missing_runs_up_to_the_concurrency_bound_at_once() {
    let max = Arc::new(AtomicUsize::new(0));
    let transport = Counting {
        in_flight: Arc::new(AtomicUsize::new(0)),
        max: Arc::clone(&max),
    };

    let report = fetch_missing(&transport, &digests(4), nz(2)).await;

    assert_eq!(report.outcome, FetchOutcome::Complete);
    assert_eq!(max.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn test_fetch_missing_serializes_at_a_bound_of_one() {
    let max = Arc::new(AtomicUsize::new(0));
    let transport = Counting {
        in_flight: Arc::new(AtomicUsize::new(0)),
        max: Arc::clone(&max),
    };

    let report = fetch_missing(&transport, &digests(4), nz(1)).await;

    assert_eq!(report.outcome, FetchOutcome::Complete);
    assert_eq!(max.load(Ordering::SeqCst), 1);
}
