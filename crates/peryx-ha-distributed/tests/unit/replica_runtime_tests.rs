use std::collections::HashMap;
use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use axum::routing::get;
use axum::{Json, Router, http::StatusCode};
use peryx_ha::{ReplicaPage, ReplicaViewApplier};
use peryx_storage::blob::{BlobStorage, Digest};
use peryx_storage::meta::MetaStore;

use crate::multi_peer::DEFAULT_SET_LIMITS;
use crate::support::TestServer;
use crate::{
    AvailabilityMetrics, BlobReference, CapacityLimited, Change, ChangePage, HttpBlobTransport, HttpPeerTransport,
    MetadataMutation, PROTOCOL_VERSION, PeerSet, ReconnectPolicy, Replica, ReplicaMonitor, RetiredPeer, TransferLimits,
    TransportError, primary_router,
};

use super::*;

const TOKEN: &str = "replication-secret";

#[derive(Default)]
struct Views {
    frontier: AtomicU64,
    committed: Mutex<Vec<peryx_ha::BlobCommit>>,
}

impl ReplicaViewApplier for Views {
    fn apply(&self, page: ReplicaPage, _changed_keys: &[String]) {
        self.frontier.store(page.serial, Ordering::Relaxed);
    }

    fn apply_blob_commit(&self, committed: &[peryx_ha::BlobCommit]) {
        self.committed
            .lock()
            .expect("the commit log is usable")
            .extend_from_slice(committed);
    }

    fn readable_frontier(&self) -> u64 {
        self.frontier.load(Ordering::Relaxed)
    }

    fn publish_applied_frontier(&self, serial: u64) {
        self.frontier.store(serial, Ordering::Relaxed);
    }
}

/// Blocks inside the frontier read so a reader can take a snapshot with a cycle in flight. Its
/// readable frontier stays at zero, standing in for the blob view a failed pass left behind.
struct PausedViews {
    arrived: mpsc::SyncSender<()>,
    release: Mutex<mpsc::Receiver<()>>,
}

impl ReplicaViewApplier for PausedViews {
    fn apply(&self, _page: ReplicaPage, _changed_keys: &[String]) {}

    fn apply_blob_commit(&self, _committed: &[peryx_ha::BlobCommit]) {}

    fn readable_frontier(&self) -> u64 {
        self.arrived.send(()).expect("the reader waits for the frontier read");
        self.release
            .lock()
            .expect("the release channel is usable")
            .recv()
            .expect("the reader releases the cycle");
        0
    }

    fn publish_applied_frontier(&self, _serial: u64) {}
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

fn metadata_peers(bases: &[(&str, &str)]) -> PeerSet<HttpPeerTransport> {
    let mut peers = PeerSet::new(DEFAULT_SET_LIMITS, bounded_policy(3));
    for (source, base) in bases {
        peers.join(
            *source,
            HttpPeerTransport::new(base, TOKEN, TransferLimits::default(), Duration::from_secs(1)).unwrap(),
            0,
        );
    }
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
    views: Arc<dyn ReplicaViewApplier>,
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
    assert!(monitor.snapshot().is_ready());
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
    assert_eq!(
        apply_error.snapshot().readiness_gaps(),
        vec!["sync_error", "frontier_lag"]
    );

    let incompatible = cycle_with_page(ChangePage {
        version: PROTOCOL_VERSION + 1,
        source: "primary".to_owned(),
        after: 0,
        current_serial: 1,
        changes: Vec::new(),
    })
    .await;
    assert_eq!(
        incompatible.snapshot().readiness_gaps(),
        vec!["incompatible_schema", "retired_peers", "frontier_lag"]
    );
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
    assert_eq!(monitor.snapshot().readiness_gaps(), vec!["sync_error", "frontier_lag"]);
}

/// A primary whose only page names a blob it does not hold, so the replica's blob plane fails while
/// its metadata plane reaches the primary's serial.
async fn primary_missing_a_blob(dir: &tempfile::TempDir) -> TestServer {
    let (source_meta, source_blobs) = stores(dir);
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
    TestServer::start(primary_router("primary", TOKEN, source_meta, source_blobs).unwrap()).await
}

#[tokio::test]
async fn test_cycle_records_a_blob_plane_failure_after_metadata_commits() {
    let source_dir = tempfile::tempdir().unwrap();
    let server = primary_missing_a_blob(&source_dir).await;
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
    let observation = monitor.snapshot();
    assert_eq!(observation.errors, 1);
    assert_eq!(observation.primary_serial, Some(1));
    assert_eq!(observation.readiness_gaps(), vec!["blob_plane"]);
}

/// The readiness answer a reader takes while a cycle runs must describe one pass. Before the fix the
/// loop published the metadata outcome, then the blob outcome, then the frontier, so a probe landing
/// between them read a caught-up replica whose blob view was still behind and returned it to service.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_a_cycle_in_flight_never_publishes_half_of_its_result() {
    let source_dir = tempfile::tempdir().unwrap();
    let server = primary_missing_a_blob(&source_dir).await;
    let target_dir = tempfile::tempdir().unwrap();
    let (meta, blobs) = stores(&target_dir);
    let (arrived, arrivals) = mpsc::sync_channel(1);
    let (releases, release) = mpsc::channel();
    let monitor = Arc::new(ReplicaMonitor::new(0));
    let mut replica = replica(
        meta,
        blobs,
        metadata(&server.url),
        blob_transport(&server.url),
        Arc::new(PausedViews {
            arrived,
            release: Mutex::new(release),
        }),
        Arc::clone(&monitor),
    );

    let cycle = tokio::spawn(async move { replica.cycle().await });
    arrivals.recv().expect("the cycle reaches its frontier read");
    let during = monitor.snapshot();
    releases.send(()).expect("the cycle resumes");
    assert!(cycle.await.unwrap().unwrap());

    assert_eq!(during.serial, 0);
    assert_eq!(during.primary_serial, None);
    assert_eq!(during.errors, 0);
    assert_eq!(during.readiness_gaps(), vec!["frontier_lag"]);

    let after = monitor.snapshot();
    assert_eq!(after.serial, 1);
    assert_eq!(after.primary_serial, Some(1));
    assert_eq!(after.readable_serial, 0);
    assert_eq!(after.readiness_gaps(), vec!["blob_plane", "readable_lag"]);
}

/// Sleeping through the window rather than advancing through it is what lets the loop run between timer
/// firings: a single `advance` jump leaves the loop parked after one cycle, so the retry the name claims
/// never happens. The default policy backs off from 100 ms doubling, so one second covers the cycles at
/// 0, 100 ms, 300 ms and 700 ms.
// paused-clock-safe: the peer set is empty, so each cycle fails before it reaches a transport
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
    tokio::time::sleep(Duration::from_secs(1)).await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());

    assert_eq!(monitor.snapshot().errors, 4);
    assert_eq!(monitor.snapshot().readiness_gaps(), vec!["sync_error", "frontier_lag"]);
}

#[tokio::test]
async fn test_cycle_reports_a_fully_retired_peer_set() {
    let server = TestServer::start(Router::new().route(
        "/+replication/v1/changes",
        get(|| async { StatusCode::TOO_MANY_REQUESTS }),
    ))
    .await;
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

    assert_eq!(replica.cycle().await, Err(TransportError::Disconnected));
    assert_eq!(
        monitor.snapshot().readiness_gaps(),
        vec!["sync_error", "retired_peers", "frontier_lag"]
    );
    assert!(monitor.snapshot().fully_retired);
    assert_eq!(
        monitor.snapshot().retired,
        vec![RetiredPeer {
            source: "primary".to_owned(),
            reason: "bad_status",
        }]
    );
}

#[tokio::test]
async fn test_cycle_keeps_partial_retirement_distinct_from_sync_error() {
    let retired_server = TestServer::start(Router::new().route(
        "/+replication/v1/changes",
        get(|| async { StatusCode::TOO_MANY_REQUESTS }),
    ))
    .await;
    let backing_off_server = TestServer::start(Router::new().route(
        "/+replication/v1/changes",
        get(|| async { StatusCode::SERVICE_UNAVAILABLE }),
    ))
    .await;
    let dir = tempfile::tempdir().unwrap();
    let (meta, blobs) = stores(&dir);
    let monitor = Arc::new(ReplicaMonitor::new(0));
    let mut replica = replica(
        meta,
        blobs,
        metadata_peers(&[
            ("retired", &retired_server.url),
            ("backing-off", &backing_off_server.url),
        ]),
        blob_transport(&retired_server.url),
        Arc::new(Views::default()),
        Arc::clone(&monitor),
    );

    assert_eq!(replica.cycle().await, Err(TransportError::Disconnected));
    assert_eq!(monitor.snapshot().readiness_gaps(), vec!["sync_error", "frontier_lag"]);
    assert!(!monitor.snapshot().fully_retired);
    assert_eq!(
        monitor.snapshot().retired,
        vec![RetiredPeer {
            source: "retired".to_owned(),
            reason: "bad_status",
        }]
    );
}

/// A primary holding one blob, referenced by a change carrying `keys`.
async fn primary_with_a_blob(dir: &tempfile::TempDir, bytes: &[u8], keys: &[&str]) -> (TestServer, Digest) {
    let (source_meta, source_blobs) = stores(dir);
    let digest = source_blobs.put_bytes(bytes).await.unwrap();
    Replica::new(&source_meta, NonZeroUsize::new(4).unwrap())
        .apply_page(ChangePage {
            version: PROTOCOL_VERSION,
            source: "seed".to_owned(),
            after: 0,
            current_serial: 1,
            changes: vec![Change {
                serial: 1,
                event: b"event-1".to_vec(),
                metadata: keys
                    .iter()
                    .map(|key| MetadataMutation::Put {
                        key: (*key).to_owned(),
                        value: b"record".to_vec(),
                    })
                    .collect(),
                blobs: vec![BlobReference {
                    sha256: digest.as_str().to_owned(),
                    size: bytes.len() as u64,
                }],
            }],
        })
        .unwrap();
    let server = TestServer::start(primary_router("primary", TOKEN, source_meta, source_blobs).unwrap()).await;
    (server, digest)
}

async fn committed_over_a_cycle(bytes: &[u8], keys: &[&str]) -> Vec<peryx_ha::BlobCommit> {
    let source_dir = tempfile::tempdir().unwrap();
    let (server, _digest) = primary_with_a_blob(&source_dir, bytes, keys).await;
    let target_dir = tempfile::tempdir().unwrap();
    let (meta, blobs) = stores(&target_dir);
    let views = Arc::new(Views::default());
    let mut replica = replica(
        meta,
        blobs,
        metadata(&server.url),
        blob_transport(&server.url),
        views.clone(),
        Arc::new(ReplicaMonitor::new(0)),
    );

    assert!(replica.cycle().await.unwrap());
    let committed = views.committed.lock().expect("the commit log is usable");
    committed.clone()
}

/// A journal record carries the rows and the blob references of one serial together, so a replica
/// already holds the association between a blob it just fetched and the resources whose records named
/// it. Nothing replicates the association, derives it from a locator this node may not hold, or carries
/// it in the pull protocol.
#[tokio::test]
async fn test_a_committed_blob_carries_the_keys_its_record_named() {
    let committed = committed_over_a_cycle(b"a-replicated-artifact", &["resource", "other"]).await;

    assert_eq!(
        committed,
        vec![peryx_ha::BlobCommit {
            digest: Digest::of(b"a-replicated-artifact").as_str().to_owned(),
            keys: vec!["other".to_owned(), "resource".to_owned()],
        }]
    );
}

/// A record naming a blob and no row leaves the replica unable to say what the bytes belong to. The
/// commit still reports, with no keys, so the applier retires the whole view rather than passing over a
/// document that still reports the bytes as absent.
#[tokio::test]
async fn test_a_committed_blob_its_record_cannot_name_reports_no_keys() {
    let committed = committed_over_a_cycle(b"an-unnamed-artifact", &[]).await;

    assert_eq!(
        committed,
        vec![peryx_ha::BlobCommit {
            digest: Digest::of(b"an-unnamed-artifact").as_str().to_owned(),
            keys: Vec::new(),
        }]
    );
}

/// A source that has installed a checkpoint holds no records below its serial, which is what a pruned
/// writer will look like. A replica reading from it is refused, recovers through the checkpoint, and
/// stands at the serial the manifest named.
#[tokio::test]
async fn test_a_cycle_recovers_through_a_checkpoint_when_the_source_refuses_the_cursor() {
    let source_dir = tempfile::tempdir().unwrap();
    let (source_meta, source_blobs) = stores(&source_dir);
    for index in 0..6 {
        source_meta
            .commit_driver_txn(|txn| {
                txn.put(&format!("pypi\u{0}p\u{0}hosted/pkg{index:04}"), b"display")
                    .map(|()| ((), vec![b"{}".to_vec()]))
            })
            .unwrap();
    }
    let identity = peryx_storage::meta::CheckpointIdentity {
        source: "primary".to_owned(),
        protocol_version: PROTOCOL_VERSION,
        schema_version: 1,
    };
    let manifest = source_meta.publish_checkpoint(identity.clone()).unwrap();
    // The source becomes a node that installed the checkpoint, so its journal starts above the cursor
    // a fresh replica asks from.
    let installed_dir = tempfile::tempdir().unwrap();
    let (installed, _installed_blobs) = stores(&installed_dir);
    installed.begin_checkpoint_transfer(&manifest).unwrap();
    let whole = source_meta
        .checkpoint_chunk(&peryx_storage::meta::CheckpointCursor::start(), 1 << 20)
        .unwrap();
    installed
        .stage_checkpoint_chunk(&manifest, 0, &whole.bytes, "done")
        .unwrap()
        .unwrap();
    installed
        .install_staged_checkpoint("replication\u{0}state", br#"{"source":"primary","serial":6}"#)
        .unwrap();
    let server = TestServer::start(primary_router("primary", TOKEN, installed, source_blobs).unwrap()).await;

    let target_dir = tempfile::tempdir().unwrap();
    let (meta, blobs) = stores(&target_dir);
    let mut replica = replica(
        meta.clone(),
        blobs,
        metadata(&server.url),
        blob_transport(&server.url),
        Arc::new(Views::default()),
        Arc::new(ReplicaMonitor::new(0)),
    );

    replica.cycle().await.unwrap();

    assert_eq!(meta.current_serial().unwrap(), manifest.serial);
    assert_eq!(
        meta.get_driver_value("pypi\u{0}p\u{0}hosted/pkg0000")
            .unwrap()
            .as_deref(),
        Some(&b"display"[..])
    );
}

fn refused_below_floor(source: &str) -> crate::RetiredPeer {
    crate::RetiredPeer {
        source: source.to_owned(),
        reason: "checkpoint_required",
    }
}

/// A source the peer set no longer holds is passed over. Nothing removes a member mid-cycle today, so
/// this is what keeps the recovery from depending on that staying true.
#[tokio::test]
async fn test_a_recovery_skips_a_source_the_peer_set_no_longer_holds() {
    let source_dir = tempfile::tempdir().unwrap();
    let (source_meta, source_blobs) = stores(&source_dir);
    let server = TestServer::start(primary_router("primary", TOKEN, source_meta, source_blobs).unwrap()).await;
    let target_dir = tempfile::tempdir().unwrap();
    let (meta, blobs) = stores(&target_dir);
    let mut replica = replica(
        meta,
        blobs,
        metadata(&server.url),
        blob_transport(&server.url),
        Arc::new(Views::default()),
        Arc::new(ReplicaMonitor::new(0)),
    );

    assert!(
        !replica
            .recover_retired_below_floor(&[refused_below_floor("absent")])
            .await
    );
}

/// A source that refused the cursor and publishes no checkpoint leaves the replica where it was, with
/// the failure reported rather than a half-installed state.
#[tokio::test]
async fn test_a_recovery_from_a_source_without_a_checkpoint_reports_no_progress() {
    let source_dir = tempfile::tempdir().unwrap();
    let (source_meta, source_blobs) = stores(&source_dir);
    let server = TestServer::start(primary_router("primary", TOKEN, source_meta, source_blobs).unwrap()).await;
    let target_dir = tempfile::tempdir().unwrap();
    let (meta, blobs) = stores(&target_dir);
    let before = meta.current_serial().unwrap();
    let mut replica = replica(
        meta.clone(),
        blobs,
        metadata(&server.url),
        blob_transport(&server.url),
        Arc::new(Views::default()),
        Arc::new(ReplicaMonitor::new(0)),
    );

    assert!(
        !replica
            .recover_retired_below_floor(&[refused_below_floor("primary")])
            .await
    );
    assert_eq!(meta.current_serial().unwrap(), before);
}
