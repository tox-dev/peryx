use std::collections::HashMap;
use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::routing::get;
use axum::{Json, Router};
use peryx_ha::{ReplicaPage, ReplicaViewApplier};
use peryx_storage::blob::{BlobStorage, Digest};
use peryx_storage::meta::MetaStore;

use crate::multi_peer::DEFAULT_SET_LIMITS;
use crate::support::TestServer;
use crate::{
    AvailabilityMetrics, BlobReference, CapacityLimited, Change, ChangePage, HttpBlobTransport, HttpPeerTransport,
    MetadataMutation, PROTOCOL_VERSION, PeerSet, ReconnectPolicy, Replica, ReplicaMonitor, TransferLimits,
    TransportError, primary_router,
};

use super::*;

const TOKEN: &str = "replication-secret";

#[derive(Default)]
struct Views(AtomicU64);

impl ReplicaViewApplier for Views {
    fn apply(&self, page: ReplicaPage, _changed_keys: &[String]) {
        self.0.store(page.serial, Ordering::Relaxed);
    }

    fn readable_frontier(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    fn publish_applied_frontier(&self, serial: u64) {
        self.0.store(serial, Ordering::Relaxed);
    }
}

fn stores(dir: &tempfile::TempDir) -> (MetaStore, BlobStorage) {
    (
        MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        BlobStorage::filesystem(dir.path().join("blobs")),
    )
}

fn blob_transport(base: &str) -> CapacityLimited<HttpBlobTransport> {
    CapacityLimited::new(
        HttpBlobTransport::new(base, TOKEN, TransferLimits::default(), Duration::from_secs(1)).unwrap(),
        REPLICA_BLOB_FETCH_CONCURRENCY,
    )
}

fn metadata(base: &str) -> PeerSet<HttpPeerTransport> {
    let mut peers = PeerSet::new(DEFAULT_SET_LIMITS, ReconnectPolicy::default());
    peers.join(
        "primary",
        HttpPeerTransport::new(base, TOKEN, TransferLimits::default(), Duration::from_secs(1)).unwrap(),
        0,
    );
    peers
}

fn page_router(page: ChangePage) -> Router {
    Router::new().route(
        "/+replication/v1/changes",
        get(move || {
            let page = page.clone();
            async move { Json(page) }
        }),
    )
}

fn replica(
    meta: MetaStore,
    blobs: BlobStorage,
    metadata: PeerSet<HttpPeerTransport>,
    transport: CapacityLimited<HttpBlobTransport>,
    views: Arc<Views>,
    monitor: Arc<ReplicaMonitor>,
) -> ReplicaLoop {
    ReplicaLoop::new(ReplicaLoopParts {
        views,
        metadata,
        policy: ReconnectPolicy::default(),
        meta,
        blobs,
        page_size: NonZeroUsize::new(4).unwrap(),
        poll_interval: Duration::from_millis(10),
        monitor,
        metrics: Arc::new(AvailabilityMetrics::default()),
        transport,
        local_dc: String::new(),
        delegates: HashMap::new(),
    })
}

fn bounded_policy(max_attempts: u32) -> ReconnectPolicy {
    ReconnectPolicy::new(
        Duration::from_millis(100),
        NonZeroU32::new(2).unwrap(),
        Duration::from_secs(30),
        NonZeroU32::new(max_attempts).unwrap(),
    )
}

#[test]
fn test_schedule_delay_resets_after_a_successful_cycle() {
    for (caught_up, expected) in [(true, Duration::from_secs(5)), (false, Duration::ZERO)] {
        let mut attempt = 4;
        assert_eq!(
            schedule_delay(
                &Ok(caught_up),
                &mut attempt,
                &ReconnectPolicy::default(),
                Duration::from_secs(5),
            ),
            expected
        );
        assert_eq!(attempt, 0);
    }
}

#[test]
fn test_schedule_delay_uses_backoff_until_the_retry_budget_is_spent() {
    for (max_attempts, expected) in [(10, Duration::from_millis(100)), (1, Duration::from_secs(5))] {
        let mut attempt = 0;
        assert_eq!(
            schedule_delay(
                &Err(TransportError::Disconnected),
                &mut attempt,
                &bounded_policy(max_attempts),
                Duration::from_secs(5),
            ),
            expected
        );
        assert_eq!(attempt, 1);
    }
}

#[tokio::test]
async fn test_cycle_applies_a_primary_page_and_runs_the_blob_plane() {
    let source_dir = tempfile::tempdir().unwrap();
    let (source_meta, source_blobs) = stores(&source_dir);
    Replica::new(&source_meta, NonZeroUsize::new(4).unwrap())
        .apply_page(ChangePage {
            version: PROTOCOL_VERSION,
            source: "seed".to_owned(),
            after: 0,
            current_serial: 1,
            changes: vec![Change {
                serial: 1,
                event: b"event-1".to_vec(),
                metadata: vec![MetadataMutation::Put {
                    key: "resource".to_owned(),
                    value: b"record".to_vec(),
                }],
                blobs: Vec::new(),
            }],
        })
        .unwrap();
    let server = TestServer::start(primary_router("primary", TOKEN, source_meta, source_blobs).unwrap()).await;
    let target_dir = tempfile::tempdir().unwrap();
    let (meta, blobs) = stores(&target_dir);
    let views = Arc::new(Views::default());
    let monitor = Arc::new(ReplicaMonitor::new(0));
    let mut replica = replica(
        meta,
        blobs,
        metadata(&server.url),
        blob_transport(&server.url),
        views,
        Arc::clone(&monitor),
    );

    assert!(replica.cycle().await.unwrap());
    assert_eq!(monitor.snapshot().primary_serial, Some(1));
    assert_eq!(monitor.readiness_gap(), None);
}

async fn cycle_with_page(page: ChangePage) -> Arc<ReplicaMonitor> {
    let server = TestServer::start(page_router(page)).await;
    let dir = tempfile::tempdir().unwrap();
    let (meta, blobs) = stores(&dir);
    let monitor = Arc::new(ReplicaMonitor::new(0));
    let mut replica = replica(
        meta,
        blobs,
        metadata(&server.url),
        blob_transport(&server.url),
        Arc::new(Views::default()),
        Arc::clone(&monitor),
    );

    assert!(replica.cycle().await.unwrap());
    monitor
}

#[tokio::test]
async fn test_cycle_records_page_apply_and_protocol_errors() {
    let apply_error = cycle_with_page(ChangePage {
        version: PROTOCOL_VERSION,
        source: "primary".to_owned(),
        after: 0,
        current_serial: 1,
        changes: vec![Change {
            serial: 1,
            event: b"event-1".to_vec(),
            metadata: vec![MetadataMutation::Put {
                key: "replication\0state".to_owned(),
                value: b"forged".to_vec(),
            }],
            blobs: Vec::new(),
        }],
    })
    .await;
    assert_eq!(apply_error.readiness_gap(), Some("sync_error"));

    let incompatible = cycle_with_page(ChangePage {
        version: PROTOCOL_VERSION + 1,
        source: "primary".to_owned(),
        after: 0,
        current_serial: 1,
        changes: Vec::new(),
    })
    .await;
    assert_eq!(incompatible.readiness_gap(), Some("incompatible_schema"));
}

#[tokio::test]
async fn test_cycle_records_an_invalid_local_replica_state() {
    let dir = tempfile::tempdir().unwrap();
    let (meta, blobs) = stores(&dir);
    meta.next_serial().unwrap();
    let monitor = Arc::new(ReplicaMonitor::new(0));
    let mut replica = replica(
        meta,
        blobs,
        PeerSet::new(DEFAULT_SET_LIMITS, ReconnectPolicy::default()),
        blob_transport("http://127.0.0.1:1/"),
        Arc::new(Views::default()),
        Arc::clone(&monitor),
    );

    assert!(replica.cycle().await.unwrap());
    assert_eq!(monitor.readiness_gap(), Some("sync_error"));
}

#[tokio::test]
async fn test_cycle_records_a_blob_plane_failure_after_metadata_commits() {
    let source_dir = tempfile::tempdir().unwrap();
    let (source_meta, source_blobs) = stores(&source_dir);
    let missing = Digest::of(b"missing");
    Replica::new(&source_meta, NonZeroUsize::new(4).unwrap())
        .apply_page(ChangePage {
            version: PROTOCOL_VERSION,
            source: "seed".to_owned(),
            after: 0,
            current_serial: 1,
            changes: vec![Change {
                serial: 1,
                event: b"event-1".to_vec(),
                metadata: Vec::new(),
                blobs: vec![BlobReference {
                    sha256: missing.as_str().to_owned(),
                    size: 7,
                }],
            }],
        })
        .unwrap();
    let server = TestServer::start(primary_router("primary", TOKEN, source_meta, source_blobs).unwrap()).await;
    let target_dir = tempfile::tempdir().unwrap();
    let (meta, blobs) = stores(&target_dir);
    let monitor = Arc::new(ReplicaMonitor::new(0));
    let mut replica = replica(
        meta,
        blobs,
        metadata(&server.url),
        blob_transport(&server.url),
        Arc::new(Views::default()),
        Arc::clone(&monitor),
    );

    assert!(replica.cycle().await.unwrap());
    assert_eq!(monitor.snapshot().errors, 1);
}

#[tokio::test(start_paused = true)]
async fn test_run_retries_a_disconnected_metadata_plane_until_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    let (meta, blobs) = stores(&dir);
    let monitor = Arc::new(ReplicaMonitor::new(0));
    let replica = replica(
        meta,
        blobs,
        PeerSet::new(DEFAULT_SET_LIMITS, ReconnectPolicy::default()),
        blob_transport("http://127.0.0.1:1/"),
        Arc::new(Views::default()),
        Arc::clone(&monitor),
    );

    let task = tokio::spawn(replica.run());
    tokio::time::advance(Duration::from_secs(1)).await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());

    assert!(monitor.snapshot().errors > 0);
    assert_eq!(monitor.readiness_gap(), Some("sync_error"));
}
