use std::collections::{BTreeSet, HashMap};
use std::num::{NonZeroU64, NonZeroUsize};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::middleware::{Next, from_fn};
use axum::response::IntoResponse as _;
use bytes::Bytes;
use peryx_ha::{ArtifactSource, BackendId, BackendLocation, BlobPlacementKey, BlobPlacementTransition, DataCenterId};
use peryx_identity::ArtifactDigest;
use peryx_storage::blob::{BlobStorage, Digest};
use peryx_storage::meta::{DriverBlobReference, JournalEntry, MetaStore};

use crate::blob::{BlobRequest, BlobTransport, CapacityLimited, LoopbackBlobSource};
use crate::blob_http::HttpBlobTransport;
use crate::blob_plane::{
    BLOB_VIEW, BlobPlaneReport, BlobSources, advance_blob_frontier, pull_outstanding, pull_referenced,
};
use crate::error::SyncError;
use crate::peer::{TransferLimits, TransportError};
use crate::support::TestServer;
use crate::{advance_blob_frontier_with_evidence, pull_outstanding_with_evidence};

const TOKEN: &str = "secret";
const BLOB_ROUTE: &str = "/+replication/v1/blobs/sha256/{digest}";

fn stores() -> (tempfile::TempDir, MetaStore, BlobStorage) {
    let dir = tempfile::tempdir().unwrap();
    let meta = crate::support::distributed_meta(dir.path().join("peryx.redb"));
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    (dir, meta, blobs)
}

fn limits() -> TransferLimits {
    TransferLimits {
        max_operations: NonZeroUsize::new(256).unwrap(),
        max_encoded_bytes: NonZeroU64::new(1 << 20).unwrap(),
    }
}

fn http_blob(base: &str) -> CapacityLimited<HttpBlobTransport> {
    CapacityLimited::new(
        HttpBlobTransport::new(base, TOKEN, limits(), Duration::from_secs(5)).unwrap(),
        nz(2),
    )
}

fn loopback(digest: &Digest, bytes: &'static [u8]) -> LoopbackBlobSource {
    LoopbackBlobSource::new(HashMap::from([(digest.clone(), Bytes::from_static(bytes))]), limits())
}

fn mislabeled(digest: &Digest, content: &'static [u8]) -> LoopbackBlobSource {
    LoopbackBlobSource::new(HashMap::from([(digest.clone(), Bytes::from_static(content))]), limits())
}

fn empty_source() -> LoopbackBlobSource {
    LoopbackBlobSource::new(HashMap::new(), limits())
}

fn seed_verified_placement(meta: &MetaStore, digest: &Digest, dc: &str, size: u64) {
    seed_verified_placement_on(meta, digest, dc, "filesystem", size);
}

fn seed_verified_placement_on(meta: &MetaStore, digest: &Digest, dc: &str, backend: &str, size: u64) {
    let artifact = ArtifactDigest::from_sha256(digest.as_str()).unwrap();
    let key = BlobPlacementKey {
        digest: artifact.clone(),
        backend: BackendId::new(backend).unwrap(),
        data_center: DataCenterId::new(dc).unwrap(),
        location: BackendLocation::new(format!("{backend}/{}", digest.as_str())).unwrap(),
    };
    crate::apply_blob_placement(meta, &key, &BlobPlacementTransition::Stage, 1, 10).unwrap();
    crate::apply_blob_placement(
        meta,
        &key,
        &BlobPlacementTransition::Verify {
            attempt: 1,
            observed: artifact,
            size,
        },
        1,
        20,
    )
    .unwrap();
}

fn nz(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}

async fn seed_local(blobs: &BlobStorage, digest: &Digest, bytes: &'static [u8]) {
    let mut write = blobs.begin().await.unwrap();
    write.write_chunk(Bytes::from_static(bytes)).await.unwrap();
    write.commit(digest).await.unwrap();
}

struct Faulty(TransportError);

#[async_trait]
impl BlobTransport for Faulty {
    async fn blob_size(&self, _digest: &Digest) -> Result<Option<u64>, TransportError> {
        Err(self.0.clone())
    }

    async fn fetch_blob(&self, _request: BlobRequest) -> Result<Vec<u8>, TransportError> {
        Err(self.0.clone())
    }
}

#[tokio::test]
async fn test_pull_referenced_fetches_absent_blobs_over_http_and_marks_them_local() {
    let (_dir, meta, blobs) = stores();
    let bytes = b"artifact";
    let digest = Digest::of(bytes);
    let remote_dir = tempfile::tempdir().unwrap();
    let remote_blobs = BlobStorage::filesystem(remote_dir.path().join("blobs"));
    remote_blobs.put_bytes(bytes).await.unwrap();
    let server = TestServer::start(
        crate::primary_router(
            "remote",
            TOKEN,
            crate::support::distributed_meta(remote_dir.path().join("peryx.redb")),
            remote_blobs,
        )
        .unwrap(),
    )
    .await;
    let source = http_blob(&server.url);

    let report = pull_referenced(&source, &blobs, &meta, &[(digest.clone(), bytes.len() as u64)], nz(2))
        .await
        .unwrap();

    assert_eq!(report, BlobPlaneReport { fetched: 1, pending: 0 });
    assert!(blobs.head(&digest).await.unwrap().is_some());
    assert!(blobs.verify(&digest).await.unwrap());
    let placement = meta.get_artifact_placement(digest.as_str()).unwrap().unwrap();
    assert!(placement.availability.is_local());
}

#[tokio::test]
async fn test_pull_referenced_skips_a_present_blob() {
    let (_dir, meta, blobs) = stores();
    let bytes = b"present";
    let digest = Digest::of(bytes);
    seed_local(&blobs, &digest, bytes).await;
    let source = Faulty(TransportError::BlobNotFound {
        digest: digest.as_str().to_owned(),
    });

    let report = pull_referenced(&source, &blobs, &meta, &[(digest.clone(), bytes.len() as u64)], nz(2))
        .await
        .unwrap();

    assert_eq!(report, BlobPlaneReport { fetched: 0, pending: 0 });
}

#[tokio::test]
async fn test_pull_referenced_repairs_a_present_blob_missing_its_placement() {
    let (_dir, meta, blobs) = stores();
    let bytes = b"orphaned";
    let digest = Digest::of(bytes);
    seed_local(&blobs, &digest, bytes).await;
    assert!(meta.get_artifact_placement(digest.as_str()).unwrap().is_none());
    let source = Faulty(TransportError::BlobNotFound {
        digest: digest.as_str().to_owned(),
    });

    let report = pull_referenced(&source, &blobs, &meta, &[(digest.clone(), bytes.len() as u64)], nz(2))
        .await
        .unwrap();

    assert_eq!(report, BlobPlaneReport { fetched: 0, pending: 0 });
    let placement = meta.get_artifact_placement(digest.as_str()).unwrap().unwrap();
    assert!(placement.availability.is_local());
    assert_eq!(placement.source, ArtifactSource::Proxy);
}

#[tokio::test]
async fn test_pull_referenced_repairs_a_present_blob_with_stale_placement() {
    let (_dir, meta, blobs) = stores();
    let bytes = b"stale-record";
    let digest = Digest::of(bytes);
    seed_local(&blobs, &digest, bytes).await;
    let stale = crate::record_artifact_placement(&meta, digest.as_str(), ArtifactSource::Hosted, false).unwrap();
    assert!(!stale.availability.is_local());
    let source = Faulty(TransportError::BlobNotFound {
        digest: digest.as_str().to_owned(),
    });

    pull_referenced(&source, &blobs, &meta, &[(digest.clone(), bytes.len() as u64)], nz(2))
        .await
        .unwrap();

    let placement = meta.get_artifact_placement(digest.as_str()).unwrap().unwrap();
    assert!(placement.availability.is_local());
    assert_eq!(placement.source, ArtifactSource::Hosted);
}

#[tokio::test]
async fn test_pull_referenced_leaves_an_already_local_placement_untouched() {
    let (_dir, meta, blobs) = stores();
    let bytes = b"already-recorded";
    let digest = Digest::of(bytes);
    seed_local(&blobs, &digest, bytes).await;
    crate::record_artifact_placement(&meta, digest.as_str(), ArtifactSource::Hosted, true).unwrap();
    let source = Faulty(TransportError::BlobNotFound {
        digest: digest.as_str().to_owned(),
    });

    pull_referenced(&source, &blobs, &meta, &[(digest.clone(), bytes.len() as u64)], nz(2))
        .await
        .unwrap();

    let placement = meta.get_artifact_placement(digest.as_str()).unwrap().unwrap();
    assert!(placement.availability.is_local());
    assert_eq!(placement.source, ArtifactSource::Hosted);
}

#[tokio::test]
async fn test_pull_referenced_leaves_a_backpressured_blob_pending() {
    let (_dir, meta, blobs) = stores();
    let digest = Digest::of(b"busy");
    let source = Faulty(TransportError::AtCapacity);

    let report = pull_referenced(&source, &blobs, &meta, &[(digest, 4)], nz(2))
        .await
        .unwrap();

    assert_eq!(report, BlobPlaneReport { fetched: 0, pending: 1 });
}

#[tokio::test]
async fn test_pull_referenced_surfaces_a_terminal_fetch_failure() {
    let (_dir, meta, blobs) = stores();
    let digest = Digest::of(b"gone");
    let source = Faulty(TransportError::BlobNotFound {
        digest: digest.as_str().to_owned(),
    });

    let error = pull_referenced(&source, &blobs, &meta, &[(digest.clone(), 4)], nz(2))
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        SyncError::BlobFetchFailed { reason, digest: failed }
            if reason == "blob_not_found" && failed == digest.as_str()
    ));
}

#[tokio::test]
async fn test_pull_referenced_fails_closed_on_a_wrong_sized_blob() {
    let (_dir, meta, blobs) = stores();
    let bytes = b"artifact";
    let digest = Digest::of(bytes);
    let source = loopback(&digest, bytes);

    let error = pull_referenced(&source, &blobs, &meta, &[(digest.clone(), 999)], nz(2))
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        SyncError::BlobSizeMismatch {
            digest: mismatched,
            expected: 999,
            actual,
        } if mismatched == digest.as_str() && actual == bytes.len() as u64
    ));
    assert!(blobs.head(&digest).await.unwrap().is_none());
}

fn seed_serial(meta: &MetaStore, after: u64, blobs: &[(&str, u64)]) {
    meta.commit_replica_txn(after, |_| {
        Ok::<_, SyncError>((
            (),
            vec![JournalEntry {
                payload: format!("event-{}", after + 1).into_bytes(),
                mutations: Vec::new(),
                blobs: blobs
                    .iter()
                    .map(|(sha256, size)| DriverBlobReference {
                        sha256: (*sha256).to_owned(),
                        size: *size,
                    })
                    .collect(),
            }],
        ))
    })
    .unwrap();
}

#[tokio::test]
async fn test_advance_holds_below_an_absent_blob_then_moves_past_it_once_present() {
    let (_dir, meta, blobs) = stores();
    let one = Digest::of(b"blob-one");
    let two = Digest::of(b"blob-two");
    seed_serial(&meta, 0, &[(one.as_str(), 8)]);
    seed_serial(&meta, 1, &[(two.as_str(), 8)]);
    seed_local(&blobs, &one, b"blob-one").await;

    assert_eq!(
        advance_blob_frontier(&meta, &blobs, nz(10), "", &BTreeSet::new())
            .await
            .unwrap(),
        1
    );
    assert_eq!(meta.view_frontier(BLOB_VIEW).unwrap(), Some(1));

    seed_local(&blobs, &two, b"blob-two").await;
    assert_eq!(
        advance_blob_frontier(&meta, &blobs, nz(10), "", &BTreeSet::new())
            .await
            .unwrap(),
        2
    );
    assert_eq!(meta.view_frontier(BLOB_VIEW).unwrap(), Some(2));

    assert_eq!(
        advance_blob_frontier(&meta, &blobs, nz(10), "", &BTreeSet::new())
            .await
            .unwrap(),
        2
    );
    assert_eq!(meta.view_frontier(BLOB_VIEW).unwrap(), Some(2));
}

#[tokio::test]
async fn test_advance_is_bounded_by_the_batch() {
    let (_dir, meta, blobs) = stores();
    for (after, content) in [(0, b"one" as &[u8]), (1, b"two"), (2, b"three")] {
        let digest = Digest::of(content);
        seed_serial(&meta, after, &[(digest.as_str(), content.len() as u64)]);
        seed_local(&blobs, &digest, content).await;
    }

    assert_eq!(
        advance_blob_frontier(&meta, &blobs, nz(2), "", &BTreeSet::new())
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        advance_blob_frontier(&meta, &blobs, nz(2), "", &BTreeSet::new())
            .await
            .unwrap(),
        3
    );
}

#[tokio::test]
async fn test_advance_lets_a_serial_without_blobs_through() {
    let (_dir, meta, blobs) = stores();
    seed_serial(&meta, 0, &[]);

    assert_eq!(
        advance_blob_frontier(&meta, &blobs, nz(10), "", &BTreeSet::new())
            .await
            .unwrap(),
        1
    );
    assert_eq!(meta.view_frontier(BLOB_VIEW).unwrap(), Some(1));
}

#[tokio::test]
async fn test_advance_fails_closed_on_an_unparseable_journal_digest() {
    let (_dir, meta, blobs) = stores();
    seed_serial(&meta, 0, &[("not-a-valid-sha256", 4)]);

    assert_eq!(
        advance_blob_frontier(&meta, &blobs, nz(10), "", &BTreeSet::new())
            .await
            .unwrap(),
        0
    );
    assert_eq!(meta.view_frontier(BLOB_VIEW).unwrap(), None);
}

#[tokio::test]
async fn test_pull_outstanding_retries_a_blob_from_the_journal_tail() {
    let (_dir, meta, blobs) = stores();
    let bytes = b"outstanding";
    let digest = Digest::of(bytes);
    seed_serial(&meta, 0, &[(digest.as_str(), bytes.len() as u64)]);
    let source = loopback(&digest, bytes);
    let delegates = HashMap::new();
    let sources = BlobSources {
        simple: &source,
        delegates: &delegates,
        local_dc: "",
    };

    let report = pull_outstanding(&sources, &meta, &blobs, nz(10), nz(2)).await.unwrap();

    assert_eq!(report, BlobPlaneReport { fetched: 1, pending: 0 });
    assert!(blobs.head(&digest).await.unwrap().is_some());
    let placement = meta.get_artifact_placement(digest.as_str()).unwrap().unwrap();
    assert!(placement.availability.is_local());
}

#[tokio::test]
async fn test_pull_outstanding_skips_a_present_tail_blob() {
    let (_dir, meta, blobs) = stores();
    let bytes = b"already-here";
    let digest = Digest::of(bytes);
    seed_serial(&meta, 0, &[(digest.as_str(), bytes.len() as u64)]);
    seed_local(&blobs, &digest, bytes).await;
    let source = Faulty(TransportError::BlobNotFound {
        digest: digest.as_str().to_owned(),
    });
    let delegates = HashMap::new();
    let sources = BlobSources {
        simple: &source,
        delegates: &delegates,
        local_dc: "",
    };

    let report = pull_outstanding(&sources, &meta, &blobs, nz(10), nz(2)).await.unwrap();

    assert_eq!(report, BlobPlaneReport { fetched: 0, pending: 0 });
}

#[tokio::test]
async fn test_pull_outstanding_repairs_a_present_tail_blob_missing_its_placement() {
    let (_dir, meta, blobs) = stores();
    let bytes = b"tail-orphan";
    let digest = Digest::of(bytes);
    seed_serial(&meta, 0, &[(digest.as_str(), bytes.len() as u64)]);
    seed_local(&blobs, &digest, bytes).await;
    assert!(meta.get_artifact_placement(digest.as_str()).unwrap().is_none());
    let source = http_blob("http://127.0.0.1:1");
    let delegates = HashMap::new();
    let sources = BlobSources {
        simple: &source,
        delegates: &delegates,
        local_dc: "",
    };

    let report = pull_outstanding(&sources, &meta, &blobs, nz(10), nz(2)).await.unwrap();

    assert_eq!(report, BlobPlaneReport { fetched: 0, pending: 0 });
    let placement = meta.get_artifact_placement(digest.as_str()).unwrap().unwrap();
    assert!(placement.availability.is_local());
}

#[tokio::test]
async fn test_pull_outstanding_ranges_a_multi_placement_blob_across_sources() {
    let (_dir, meta, blobs) = stores();
    let bytes = b"multi-source-blob";
    let digest = Digest::of(bytes);
    seed_serial(&meta, 0, &[(digest.as_str(), bytes.len() as u64)]);
    seed_verified_placement(&meta, &digest, "dc-a", bytes.len() as u64);
    seed_verified_placement(&meta, &digest, "dc-b", bytes.len() as u64);
    let down = empty_source();
    let up = loopback(&digest, bytes);
    let delegates = HashMap::from([("dc-a".to_owned(), down), ("dc-b".to_owned(), up)]);
    let sources = BlobSources {
        simple: &empty_source(),
        delegates: &delegates,
        local_dc: "dc-a",
    };

    let report = pull_outstanding(&sources, &meta, &blobs, nz(10), nz(2)).await.unwrap();

    assert_eq!(report, BlobPlaneReport { fetched: 1, pending: 0 });
    assert!(blobs.verify(&digest).await.unwrap());
    let placement = meta.get_artifact_placement(digest.as_str()).unwrap().unwrap();
    assert!(placement.availability.is_local());
}

#[tokio::test]
async fn test_pull_outstanding_leaves_a_ranged_blob_pending_when_every_source_is_down() {
    let (_dir, meta, blobs) = stores();
    let bytes = b"unreachable";
    let digest = Digest::of(bytes);
    seed_serial(&meta, 0, &[(digest.as_str(), bytes.len() as u64)]);
    seed_verified_placement(&meta, &digest, "dc-a", bytes.len() as u64);
    seed_verified_placement(&meta, &digest, "dc-b", bytes.len() as u64);
    let delegates = HashMap::from([("dc-a".to_owned(), empty_source()), ("dc-b".to_owned(), empty_source())]);
    let sources = BlobSources {
        simple: &empty_source(),
        delegates: &delegates,
        local_dc: "dc-a",
    };

    let report = pull_outstanding(&sources, &meta, &blobs, nz(10), nz(2)).await.unwrap();

    assert_eq!(report, BlobPlaneReport { fetched: 0, pending: 1 });
    assert!(blobs.head(&digest).await.unwrap().is_none());
}

#[tokio::test]
async fn test_pull_outstanding_rejects_a_ranged_blob_a_peer_corrupts() {
    let (_dir, meta, blobs) = stores();
    let bytes = b"12345";
    let digest = Digest::of(bytes);
    seed_serial(&meta, 0, &[(digest.as_str(), bytes.len() as u64)]);
    seed_verified_placement(&meta, &digest, "dc-a", bytes.len() as u64);
    seed_verified_placement(&meta, &digest, "dc-b", bytes.len() as u64);
    let delegates = HashMap::from([
        ("dc-a".to_owned(), mislabeled(&digest, b"67890")),
        ("dc-b".to_owned(), mislabeled(&digest, b"67890")),
    ]);
    let sources = BlobSources {
        simple: &empty_source(),
        delegates: &delegates,
        local_dc: "dc-a",
    };

    let error = pull_outstanding(&sources, &meta, &blobs, nz(10), nz(2))
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        SyncError::BlobFetchFailed { reason, digest: failed }
            if reason == "reassembly_failed" && failed == digest.as_str()
    ));
    assert!(blobs.head(&digest).await.unwrap().is_none());
}

#[tokio::test]
async fn test_pull_outstanding_rejects_a_ranged_range_of_the_wrong_length() {
    let (_dir, meta, blobs) = stores();
    let content = b"12345";
    let digest = Digest::of(content);
    seed_serial(&meta, 0, &[(digest.as_str(), 10)]);
    seed_verified_placement(&meta, &digest, "dc-a", 10);
    seed_verified_placement(&meta, &digest, "dc-b", 10);
    let delegates = HashMap::from([
        ("dc-a".to_owned(), loopback(&digest, content)),
        ("dc-b".to_owned(), loopback(&digest, content)),
    ]);
    let sources = BlobSources {
        simple: &empty_source(),
        delegates: &delegates,
        local_dc: "dc-a",
    };

    let error = pull_outstanding(&sources, &meta, &blobs, nz(10), nz(2))
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        SyncError::BlobFetchFailed { reason, digest: failed }
            if reason == "range_length_mismatch" && failed == digest.as_str()
    ));
}

#[tokio::test]
async fn test_pull_outstanding_counts_two_placements_in_one_data_center_as_one_source() {
    let (_dir, meta, blobs) = stores();
    let bytes = b"one-data-center";
    let digest = Digest::of(bytes);
    seed_serial(&meta, 0, &[(digest.as_str(), bytes.len() as u64)]);
    seed_verified_placement_on(&meta, &digest, "dc-a", "filesystem", bytes.len() as u64);
    seed_verified_placement_on(&meta, &digest, "dc-a", "s3", bytes.len() as u64);
    let simple = loopback(&digest, bytes);
    let delegates = HashMap::from([("dc-a".to_owned(), empty_source())]);
    let sources = BlobSources {
        simple: &simple,
        delegates: &delegates,
        local_dc: "dc-a",
    };

    let report = pull_outstanding(&sources, &meta, &blobs, nz(10), nz(2)).await.unwrap();

    assert_eq!(report, BlobPlaneReport { fetched: 1, pending: 0 });
    assert!(blobs.verify(&digest).await.unwrap());
}

#[tokio::test]
async fn test_pull_outstanding_takes_the_whole_blob_path_for_a_single_placement() {
    let (_dir, meta, blobs) = stores();
    let bytes = b"single-source";
    let digest = Digest::of(bytes);
    seed_serial(&meta, 0, &[(digest.as_str(), bytes.len() as u64)]);
    seed_verified_placement(&meta, &digest, "dc-a", bytes.len() as u64);
    let simple = loopback(&digest, bytes);
    let delegates = HashMap::from([("dc-a".to_owned(), empty_source())]);
    let sources = BlobSources {
        simple: &simple,
        delegates: &delegates,
        local_dc: "dc-a",
    };

    let report = pull_outstanding(&sources, &meta, &blobs, nz(10), nz(2)).await.unwrap();

    assert_eq!(report, BlobPlaneReport { fetched: 1, pending: 0 });
    assert!(blobs.verify(&digest).await.unwrap());
}

#[tokio::test]
async fn test_pull_outstanding_reports_a_peer_that_does_not_serve_the_blob() {
    let (_dir, meta, blobs) = stores();
    let bytes = b"peer-held";
    let digest = Digest::of(bytes);
    seed_serial(&meta, 0, &[(digest.as_str(), bytes.len() as u64)]);
    seed_verified_placement(&meta, &digest, "dc-b", bytes.len() as u64);
    let server = TestServer::start(crate::support::http_contract::fixed_get(BLOB_ROUTE, || {
        (StatusCode::NOT_FOUND, [("x-peryx-blob-result", "not-found")]).into_response()
    }))
    .await;
    let simple = http_blob(&server.url);
    let delegates = HashMap::from([("dc-b".to_owned(), http_blob(&server.url))]);
    let sources = BlobSources {
        simple: &simple,
        delegates: &delegates,
        local_dc: "dc-a",
    };

    let error = pull_outstanding(&sources, &meta, &blobs, nz(10), nz(2))
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        SyncError::BlobFetchFailed {
            reason: "blob_not_found",
            ..
        }
    ));
    assert!(blobs.head(&digest).await.unwrap().is_none());
}

#[tokio::test]
async fn test_pull_outstanding_defers_only_after_the_peer_serves_bytes() {
    let (_dir, meta, blobs) = stores();
    let bytes = b"peer-held";
    let digest = Digest::of(bytes);
    seed_serial(&meta, 0, &[(digest.as_str(), bytes.len() as u64)]);
    seed_verified_placement(&meta, &digest, "dc-b", bytes.len() as u64);
    let simple = loopback(&digest, bytes);
    let delegates = HashMap::from([("dc-b".to_owned(), loopback(&digest, bytes))]);
    let sources = BlobSources {
        simple: &simple,
        delegates: &delegates,
        local_dc: "dc-a",
    };

    let report = pull_outstanding(&sources, &meta, &blobs, nz(10), nz(2)).await.unwrap();

    assert_eq!(report, BlobPlaneReport { fetched: 0, pending: 0 });
    assert!(blobs.head(&digest).await.unwrap().is_none());
}

#[tokio::test]
async fn test_pull_outstanding_probes_once_without_downloading_the_deferred_blob() {
    let (_dir, meta, blobs) = stores();
    let bytes = b"peer-held";
    let digest = Digest::of(bytes);
    seed_serial(&meta, 0, &[(digest.as_str(), bytes.len() as u64)]);
    seed_verified_placement(&meta, &digest, "dc-b", bytes.len() as u64);
    let remote_dir = tempfile::tempdir().unwrap();
    let remote_blobs = BlobStorage::filesystem(remote_dir.path().join("blobs"));
    remote_blobs.put_bytes(bytes).await.unwrap();
    let requests = Arc::new(AtomicUsize::new(0));
    let counted_requests = Arc::clone(&requests);
    let router = crate::primary_router(
        "remote",
        TOKEN,
        crate::support::distributed_meta(remote_dir.path().join("peryx.redb")),
        remote_blobs,
    )
    .unwrap()
    .layer(from_fn(move |request: Request, next: Next| {
        let requests = Arc::clone(&counted_requests);
        async move {
            assert_eq!(request.method(), Method::HEAD);
            requests.fetch_add(1, Ordering::Relaxed);
            next.run(request).await
        }
    }));
    let server = TestServer::start(router).await;
    let simple = http_blob(&server.url);
    let delegates = HashMap::from([("dc-b".to_owned(), http_blob(&server.url))]);
    let sources = BlobSources {
        simple: &simple,
        delegates: &delegates,
        local_dc: "dc-a",
    };

    let report = pull_outstanding(&sources, &meta, &blobs, nz(10), nz(2)).await.unwrap();

    assert_eq!(report, BlobPlaneReport { fetched: 0, pending: 0 });
    assert_eq!(requests.load(Ordering::Relaxed), 1);
    assert!(blobs.head(&digest).await.unwrap().is_none());
}

#[rstest::rstest]
#[case::missing_route(TransportError::BadStatus { status: 404 }, "blob_route_unavailable")]
#[case::unauthenticated(TransportError::Unauthenticated, "unauthenticated")]
#[tokio::test]
async fn test_pull_outstanding_surfaces_a_terminal_peer_failure(
    #[case] failure: TransportError,
    #[case] reason: &'static str,
) {
    let (_dir, meta, blobs) = stores();
    let bytes = b"peer-held";
    let digest = Digest::of(bytes);
    seed_serial(&meta, 0, &[(digest.as_str(), bytes.len() as u64)]);
    seed_verified_placement(&meta, &digest, "dc-b", bytes.len() as u64);
    let simple = Faulty(failure.clone());
    let delegates = HashMap::from([("dc-b".to_owned(), Faulty(failure))]);
    let sources = BlobSources {
        simple: &simple,
        delegates: &delegates,
        local_dc: "dc-a",
    };

    let error = pull_outstanding(&sources, &meta, &blobs, nz(10), nz(2))
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        SyncError::BlobFetchFailed { reason: actual, .. } if actual == reason
    ));
}

#[tokio::test]
async fn test_pull_outstanding_leaves_a_retryable_peer_probe_pending() {
    let (_dir, meta, blobs) = stores();
    let digest = Digest::of(b"peer-held");
    seed_serial(&meta, 0, &[(digest.as_str(), 9)]);
    seed_verified_placement(&meta, &digest, "dc-b", 9);
    let server = TestServer::start(crate::support::http_contract::fixed_get(BLOB_ROUTE, || {
        StatusCode::SERVICE_UNAVAILABLE.into_response()
    }))
    .await;
    let simple = http_blob(&server.url);
    let delegates = HashMap::from([("dc-b".to_owned(), http_blob(&server.url))]);
    let sources = BlobSources {
        simple: &simple,
        delegates: &delegates,
        local_dc: "dc-a",
    };

    let report = pull_outstanding(&sources, &meta, &blobs, nz(10), nz(2)).await.unwrap();

    assert_eq!(report, BlobPlaneReport { fetched: 0, pending: 1 });
    assert!(blobs.head(&digest).await.unwrap().is_none());
}

#[tokio::test]
async fn test_pull_outstanding_rejects_peer_evidence_with_the_wrong_size() {
    let (_dir, meta, blobs) = stores();
    let digest = Digest::of(b"peer-held");
    seed_serial(&meta, 0, &[(digest.as_str(), 9)]);
    seed_verified_placement(&meta, &digest, "dc-b", 9);
    let server = TestServer::start(crate::support::http_contract::fixed_get(BLOB_ROUTE, || {
        (StatusCode::OK, [("content-length", "5")]).into_response()
    }))
    .await;
    let simple = http_blob(&server.url);
    let delegates = HashMap::from([("dc-b".to_owned(), http_blob(&server.url))]);
    let sources = BlobSources {
        simple: &simple,
        delegates: &delegates,
        local_dc: "dc-a",
    };

    let error = pull_outstanding(&sources, &meta, &blobs, nz(10), nz(2))
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        SyncError::BlobFetchFailed {
            reason: "blob_size_mismatch",
            ..
        }
    ));
    assert!(blobs.head(&digest).await.unwrap().is_none());
}

#[tokio::test]
async fn test_pull_outstanding_whole_pulls_a_peer_blob_no_delegate_reaches() {
    let (_dir, meta, blobs) = stores();
    let bytes = b"unreachable-peer";
    let digest = Digest::of(bytes);
    seed_serial(&meta, 0, &[(digest.as_str(), bytes.len() as u64)]);
    seed_verified_placement(&meta, &digest, "dc-b", bytes.len() as u64);
    let simple = loopback(&digest, bytes);
    let delegates = HashMap::new();
    let sources = BlobSources {
        simple: &simple,
        delegates: &delegates,
        local_dc: "dc-a",
    };

    let report = pull_outstanding(&sources, &meta, &blobs, nz(10), nz(2)).await.unwrap();

    assert_eq!(report, BlobPlaneReport { fetched: 1, pending: 0 });
    assert!(blobs.verify(&digest).await.unwrap());
}

#[tokio::test]
async fn test_pull_outstanding_pulls_a_peer_blob_also_placed_locally() {
    let (_dir, meta, blobs) = stores();
    let bytes = b"home-copy";
    let digest = Digest::of(bytes);
    seed_serial(&meta, 0, &[(digest.as_str(), bytes.len() as u64)]);
    seed_verified_placement(&meta, &digest, "dc-a", bytes.len() as u64);
    seed_verified_placement(&meta, &digest, "dc-b", bytes.len() as u64);
    let simple = loopback(&digest, bytes);
    let delegates = HashMap::from([("dc-b".to_owned(), empty_source())]);
    let sources = BlobSources {
        simple: &simple,
        delegates: &delegates,
        local_dc: "dc-a",
    };

    let report = pull_outstanding(&sources, &meta, &blobs, nz(10), nz(2)).await.unwrap();

    assert_eq!(report, BlobPlaneReport { fetched: 1, pending: 0 });
    assert!(blobs.verify(&digest).await.unwrap());
}

#[tokio::test]
async fn test_advance_holds_a_peer_blob_without_positive_evidence() {
    let (_dir, meta, blobs) = stores();
    let digest = Digest::of(b"peer-held");
    seed_serial(&meta, 0, &[(digest.as_str(), 9)]);
    seed_verified_placement(&meta, &digest, "dc-b", 9);
    let reachable = BTreeSet::from(["dc-b".to_owned()]);

    assert_eq!(
        advance_blob_frontier(&meta, &blobs, nz(10), "dc-a", &reachable)
            .await
            .unwrap(),
        0
    );
    assert_eq!(meta.view_frontier(BLOB_VIEW).unwrap(), None);
    assert!(blobs.head(&digest).await.unwrap().is_none());
}

#[tokio::test]
async fn test_advance_accepts_positive_peer_evidence_for_the_digest() {
    let (_dir, meta, blobs) = stores();
    let bytes = b"peer-held";
    let digest = Digest::of(bytes);
    seed_serial(&meta, 0, &[(digest.as_str(), bytes.len() as u64)]);
    seed_verified_placement(&meta, &digest, "dc-b", bytes.len() as u64);
    let simple = loopback(&digest, bytes);
    let delegates = HashMap::from([("dc-b".to_owned(), loopback(&digest, bytes))]);
    let sources = BlobSources {
        simple: &simple,
        delegates: &delegates,
        local_dc: "dc-a",
    };
    let (_, served_by_peer) = pull_outstanding_with_evidence(&sources, &meta, &blobs, nz(10), nz(2))
        .await
        .unwrap();

    assert_eq!(
        advance_blob_frontier_with_evidence(&meta, &blobs, nz(10), &served_by_peer)
            .await
            .unwrap(),
        1
    );
    assert_eq!(meta.view_frontier(BLOB_VIEW).unwrap(), Some(1));
    assert!(blobs.head(&digest).await.unwrap().is_none());
}

#[tokio::test]
async fn test_advance_holds_a_peer_blob_with_no_reachable_peer() {
    let (_dir, meta, blobs) = stores();
    let digest = Digest::of(b"peer-held");
    seed_serial(&meta, 0, &[(digest.as_str(), 9)]);
    seed_verified_placement(&meta, &digest, "dc-b", 9);

    assert_eq!(
        advance_blob_frontier(&meta, &blobs, nz(10), "dc-a", &BTreeSet::new())
            .await
            .unwrap(),
        0
    );
    assert_eq!(meta.view_frontier(BLOB_VIEW).unwrap(), None);
}

#[tokio::test]
async fn test_pull_outstanding_needs_two_resolvable_delegates_to_range() {
    let (_dir, meta, blobs) = stores();
    let bytes = b"one-delegate";
    let digest = Digest::of(bytes);
    seed_serial(&meta, 0, &[(digest.as_str(), bytes.len() as u64)]);
    seed_verified_placement(&meta, &digest, "dc-a", bytes.len() as u64);
    seed_verified_placement(&meta, &digest, "dc-b", bytes.len() as u64);
    let simple = loopback(&digest, bytes);
    let delegates = HashMap::from([("dc-a".to_owned(), empty_source())]);
    let sources = BlobSources {
        simple: &simple,
        delegates: &delegates,
        local_dc: "dc-a",
    };

    let report = pull_outstanding(&sources, &meta, &blobs, nz(10), nz(2)).await.unwrap();

    assert_eq!(report, BlobPlaneReport { fetched: 1, pending: 0 });
    assert!(blobs.verify(&digest).await.unwrap());
}
