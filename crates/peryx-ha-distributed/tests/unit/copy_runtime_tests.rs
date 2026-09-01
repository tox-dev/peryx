use std::collections::BTreeSet;
use std::num::{NonZeroU64, NonZeroUsize};
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::LoopbackBlobSource;
use crate::support::TestServer;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use axum::serve::ListenerExt as _;
use peryx_ha::{BackendLocation, BlobPlacementState, BlobPlacementStatus, DataCenterId};
use peryx_identity::ArtifactDigest;
use peryx_storage::blob::BlobStorage;
use tokio::sync::Notify;
use tokio_util::bytes::Bytes;

use super::*;

const CONTENT: &[u8] = b"cross-datacenter artifact bytes";

#[test]
fn test_task_error_preserves_details() {
    let error = task_error("copy_failed", "peer unavailable");

    assert_eq!(error.code(), "copy_failed");
    assert_eq!(error.message(), "peer unavailable");
}

fn digests(content: &[u8]) -> (Digest, ArtifactDigest) {
    let blob = Digest::of(content);
    let artifact = ArtifactDigest::from_sha256(blob.as_str()).unwrap();
    (blob, artifact)
}

fn dc(name: &str) -> DataCenterId {
    DataCenterId::new(name).unwrap()
}

fn filesystem() -> (tempfile::TempDir, BlobStore, BackendId) {
    let dir = tempfile::tempdir().unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let store = blobs.filesystem_store().unwrap().clone();
    let backend = blobs.backend_id();
    (dir, store, backend)
}

#[test]
fn test_http_copier_requires_a_remote_datacenter() {
    let (_dir, store, backend) = filesystem();

    assert!(
        CrossDcBlobCopier::http(dc("home"), HashMap::new(), "token", store, backend)
            .unwrap()
            .is_none()
    );
}

#[test]
fn test_http_copier_accepts_a_remote_datacenter() {
    let (_dir, store, backend) = filesystem();

    assert!(
        CrossDcBlobCopier::http(
            dc("home"),
            HashMap::from([("east".to_owned(), "http://peer/".to_owned())]),
            "token",
            store,
            backend,
        )
        .unwrap()
        .is_some()
    );
}

#[test]
fn test_http_copier_rejects_an_invalid_remote_address() {
    let (_dir, store, backend) = filesystem();

    assert!(
        CrossDcBlobCopier::http(
            dc("home"),
            HashMap::from([("east".to_owned(), "peer.invalid".to_owned())]),
            "token",
            store,
            backend,
        )
        .is_err()
    );
}

fn meta() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::support::distributed_meta(dir.path().join("peryx.redb"));
    (dir, store)
}

fn key(digest: &ArtifactDigest, backend: &BackendId, data_center: &str, location: &str) -> BlobPlacementKey {
    BlobPlacementKey {
        digest: digest.clone(),
        backend: backend.clone(),
        data_center: dc(data_center),
        location: BackendLocation::new(location).unwrap(),
    }
}

fn seed_verified(meta: &MetaStore, key: &BlobPlacementKey, size: u64) {
    crate::apply_blob_placement(meta, key, &BlobPlacementTransition::Stage, 1, 10).unwrap();
    crate::apply_blob_placement(
        meta,
        key,
        &BlobPlacementTransition::Verify {
            attempt: 1,
            observed: key.digest.clone(),
            size,
        },
        1,
        11,
    )
    .unwrap();
}

struct FakePeers {
    peers: HashMap<String, LoopbackBlobSource>,
}

impl FakePeers {
    fn holding(data_center: &str, digest: &Digest, content: &[u8]) -> Self {
        Self {
            peers: HashMap::from([(
                data_center.to_owned(),
                LoopbackBlobSource::new(
                    HashMap::from([(digest.clone(), Bytes::copy_from_slice(content))]),
                    TransferLimits::default(),
                ),
            )]),
        }
    }
}

impl SourceTransports for FakePeers {
    fn transport(&self, source_dc: &str) -> Option<&(dyn BlobTransport + Send + Sync)> {
        self.peers
            .get(source_dc)
            .map(|transport| transport as &(dyn BlobTransport + Send + Sync))
    }
}

#[derive(Clone)]
struct RequestGate {
    fetches: Arc<AtomicUsize>,
    started: Arc<Notify>,
    release: Arc<Notify>,
}

async fn gate_blob_request(State(gate): State<RequestGate>, request: Request, next: Next) -> Response {
    gate.fetches.fetch_add(1, Ordering::SeqCst);
    gate.started.notify_one();
    gate.release.notified().await;
    next.run(request).await
}

fn copier_with(
    local: &str,
    backend: BackendId,
    store: BlobStore,
    sources: Arc<dyn SourceTransports>,
) -> CrossDcBlobCopier {
    CrossDcBlobCopier {
        local_dc: dc(local),
        backend,
        store,
        sources,
    }
}

fn plan(digest: &ArtifactDigest, backend: &BackendId, source_dc: &str, local: &str, size: u64) -> CrossDcCopy {
    CrossDcCopy {
        target: key(digest, backend, local, digest.sha256()),
        source: key(digest, backend, source_dc, "peer/location"),
        size,
        fence: NonZeroU64::new(5).unwrap(),
    }
}

#[test]
fn test_record_reports_a_committed_transition() {
    let (_dir, meta) = meta();
    let (_store_dir, _store, backend) = filesystem();
    let (_content, artifact) = digests(CONTENT);
    let clock: Clock = Arc::new(|| 42);

    let recorded = record(
        &meta,
        &key(&artifact, &backend, "east", artifact.sha256()),
        &BlobPlacementTransition::Stage,
        5,
        &clock,
    );

    assert!(recorded.is_some());
}

#[test]
fn test_record_reports_a_rejected_transition() {
    let (_dir, meta) = meta();
    let (_store_dir, _store, backend) = filesystem();
    let (_content, artifact) = digests(CONTENT);
    for index in 0..peryx_ha::MAX_PLACEMENTS_PER_DIGEST {
        crate::apply_blob_placement(
            &meta,
            &key(&artifact, &backend, "east", &format!("loc-{index}")),
            &BlobPlacementTransition::Stage,
            5,
            0,
        )
        .unwrap();
    }
    let clock: Clock = Arc::new(|| 42);

    let recorded = record(
        &meta,
        &key(&artifact, &backend, "local", artifact.sha256()),
        &BlobPlacementTransition::Stage,
        5,
        &clock,
    );

    assert!(recorded.is_none());
}

#[rstest::rstest]
#[case::corrupt_whole_blob(
    CopyError::Fetch(TransportError::DigestMismatch { expected: "a".to_owned(), actual: "b".to_owned() }),
    BlobPlacementFailure::DigestMismatch
)]
#[case::wrong_window(
    CopyError::Fetch(TransportError::RangeMismatch { expected: "bytes 0-7".to_owned(), actual: String::new() }),
    BlobPlacementFailure::DigestMismatch
)]
#[case::wrong_length(
    CopyError::RangeLength { offset: 0, expected: 8, actual: 6 },
    BlobPlacementFailure::DigestMismatch
)]
#[case::absent_blob(
    CopyError::Fetch(TransportError::BlobNotFound { digest: "x".to_owned() }),
    BlobPlacementFailure::SourceUnavailable
)]
#[case::local_byte_cap(
    CopyError::Fetch(TransportError::FrameTooLarge { limit: 4, actual: 8 }),
    BlobPlacementFailure::TransferLimit
)]
#[case::corrupt_stage(
    CopyError::Publish(peryx_storage::blob::BlobError::digest_mismatch(&Digest::of(b"a"), &Digest::of(b"b"))),
    BlobPlacementFailure::DigestMismatch
)]
#[case::backend_refusal(
    CopyError::Publish(peryx_storage::blob::BlobError::unsupported("no")),
    BlobPlacementFailure::BackendRejected
)]
fn test_failure_class_maps_each_loss_to_its_evidence(#[case] error: CopyError, #[case] expected: BlobPlacementFailure) {
    assert_eq!(failure_class(&error), expected);
}

fn roster_transports() -> RosterTransports {
    RosterTransports::new(
        HashMap::from([("east".to_owned(), "http://peer/".to_owned())]),
        "secret",
    )
    .unwrap()
}

#[test]
fn test_roster_transport_resolves_a_rostered_peer() {
    assert!(roster_transports().transport("east").is_some());
}

#[test]
fn test_roster_transport_skips_an_unrostered_peer() {
    assert!(roster_transports().transport("absent").is_none());
}

#[test]
fn test_roster_transport_rejects_an_empty_token() {
    assert!(RosterTransports::new(HashMap::from([("east".to_owned(), "http://peer/".to_owned())]), "",).is_err());
}

fn pacing(batch: usize, limit: usize) -> PassPacing {
    PassPacing {
        batch: NonZeroUsize::new(batch).unwrap(),
        limit: NonZeroUsize::new(limit).unwrap(),
    }
}

struct Owed {
    blob: Digest,
    artifact: ArtifactDigest,
    content: &'static [u8],
}

/// Seeds one verified east placement per content and returns them in the placement index's order.
fn seed_owed(meta: &MetaStore, backend: &BackendId, contents: &[&'static [u8]]) -> Vec<Owed> {
    let mut owed: Vec<Owed> = contents
        .iter()
        .map(|content| {
            let (blob, artifact) = digests(content);
            Owed {
                blob,
                artifact,
                content,
            }
        })
        .collect();
    owed.sort_by_key(|entry| entry.artifact.canonical());
    for entry in &owed {
        seed_verified(
            meta,
            &key(&entry.artifact, backend, "east", "peer/loc"),
            entry.content.len() as u64,
        );
    }
    owed
}

fn plan_pages(
    copier: &CrossDcBlobCopier,
    meta: &MetaStore,
    pacing: PassPacing,
    cancelled: &(dyn Fn() -> bool + Send + Sync),
) -> (Vec<CrossDcCopy>, BacklogScan) {
    let mut scan = BacklogScan::resuming(None);
    let mut planned = Vec::new();
    while let Some(page) = copier.next_page(meta, &mut scan, NonZeroU64::new(5).unwrap(), pacing, cancelled) {
        planned.extend(page);
    }
    (planned, scan)
}

#[test]
fn test_backlog_scan_plans_every_owed_digest_across_pages() {
    let (_dir, meta) = meta();
    let (_store_dir, store, backend) = filesystem();
    seed_owed(&meta, &backend, &[b"blob-aa", b"blob-bb"]);
    let copier = copier_with("home", backend, store, Arc::new(FakePeers { peers: HashMap::new() }));

    let (planned, _scan) = plan_pages(&copier, &meta, pacing(1, 64), &|| false);

    assert_eq!(planned.len(), 2);
}

#[test]
fn test_backlog_scan_clears_the_cursor_once_the_index_is_exhausted() {
    let (_dir, meta) = meta();
    let (_store_dir, store, backend) = filesystem();
    seed_owed(&meta, &backend, &[b"blob-aa", b"blob-bb"]);
    let copier = copier_with("home", backend, store, Arc::new(FakePeers { peers: HashMap::new() }));

    let (_planned, scan) = plan_pages(&copier, &meta, pacing(1, 64), &|| false);

    assert_eq!(scan.resume(), None, "an exhausted scan restarts the next pass");
}

#[test]
fn test_backlog_scan_stops_at_the_per_pass_cap() {
    let (_dir, meta) = meta();
    let (_store_dir, store, backend) = filesystem();
    let owed = seed_owed(&meta, &backend, &[b"blob-aa", b"blob-bb", b"blob-cc"]);
    let copier = copier_with("home", backend, store, Arc::new(FakePeers { peers: HashMap::new() }));

    let (planned, scan) = plan_pages(&copier, &meta, pacing(1, 1), &|| false);

    let first = owed[0].artifact.canonical();
    assert_eq!(
        (planned.len(), scan.resume()),
        (1, Some(first.as_str())),
        "the cap stops the scan at a page boundary and keeps a resume cursor"
    );
}

#[test]
fn test_backlog_scan_stops_when_cancelled() {
    let (_dir, meta) = meta();
    let (_store_dir, store, backend) = filesystem();
    seed_owed(&meta, &backend, &[CONTENT]);
    let copier = copier_with("home", backend, store, Arc::new(FakePeers { peers: HashMap::new() }));

    let (planned, _scan) = plan_pages(&copier, &meta, pacing(256, 64), &|| true);

    assert!(planned.is_empty(), "a cancelled pass plans nothing");
}

fn corrupt_placement_store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    crate::support::distributed_meta(&path);
    let database = redb::Database::open(&path).unwrap();
    let transaction = database.begin_write().unwrap();
    transaction
        .open_table(redb::TableDefinition::<&str, &[u8]>::new("blob_placement"))
        .unwrap()
        .insert("invalid", b"invalid".as_slice())
        .unwrap();
    transaction.commit().unwrap();
    drop(database);
    (dir, MetaStore::open_existing(path).unwrap())
}

#[test]
fn test_backlog_scan_records_a_scan_failure() {
    let (_dir, meta) = corrupt_placement_store();
    let (_store_dir, store, backend) = filesystem();
    let copier = copier_with("home", backend, store, Arc::new(FakePeers { peers: HashMap::new() }));

    let (planned, scan) = plan_pages(&copier, &meta, pacing(1, 64), &|| false);

    assert!(planned.is_empty());
    assert_eq!(scan.error.map(|error| error.code()), Some("copy_backlog_scan"));
}

fn local_state(meta: &MetaStore, target: &BlobPlacementKey) -> BlobPlacementState {
    meta.blob_placement(target).unwrap().unwrap().state
}

#[tokio::test]
async fn test_copy_one_publishes_and_records_a_verified_copy() {
    let (_dir, meta) = meta();
    let (_store_dir, store, backend) = filesystem();
    let (blob, artifact) = digests(CONTENT);
    let clock: Clock = Arc::new(|| 7);
    let copier = copier_with(
        "home",
        backend.clone(),
        store.clone(),
        Arc::new(FakePeers::holding("east", &blob, CONTENT)),
    );
    let copy = plan(&artifact, &backend, "east", "home", CONTENT.len() as u64);
    let target = copy.target.clone();

    let recorded = copier.copy_one(&meta, &clock, copy).await;

    assert!(recorded);
    assert_eq!(store.read(&blob).unwrap(), CONTENT);
    assert_eq!(local_state(&meta, &target).status(), BlobPlacementStatus::Verified);
}

#[tokio::test]
async fn test_restart_recovers_pending_after_the_ownership_term_advances() {
    let meta_dir = tempfile::tempdir().unwrap();
    let path = meta_dir.path().join("peryx.redb");
    let meta = crate::support::distributed_meta(&path);
    let (_store_dir, store, backend) = filesystem();
    let (blob, artifact) = digests(CONTENT);
    seed_verified(
        &meta,
        &key(&artifact, &backend, "east", "peer/loc"),
        CONTENT.len() as u64,
    );
    let target = key(&artifact, &backend, "home", artifact.sha256());
    crate::apply_blob_placement(&meta, &target, &BlobPlacementTransition::Stage, 5, 10).unwrap();
    drop(meta);
    let meta = MetaStore::open_existing(path).unwrap();
    let copier = copier_with(
        "home",
        backend,
        store.clone(),
        Arc::new(FakePeers::holding("east", &blob, CONTENT)),
    );
    let clock: Clock = Arc::new(|| 20);

    let live_report = copier
        .copy_pass(&meta, &clock, 5, &|| false, NonZeroUsize::MIN)
        .await
        .unwrap();
    let recovered_report = copier
        .copy_pass(&meta, &clock, 9, &|| false, NonZeroUsize::MIN)
        .await
        .unwrap();

    assert_eq!(
        (
            live_report,
            recovered_report,
            store.read(&blob).unwrap(),
            meta.blob_placement(&target).unwrap(),
        ),
        (
            AvailabilityTaskReport::default(),
            AvailabilityTaskReport {
                processed: 1,
                changed: 1,
            },
            CONTENT.to_vec(),
            Some(peryx_ha::BlobPlacementRecord {
                key: target,
                state: BlobPlacementState::Verified {
                    size: CONTENT.len() as u64,
                },
                fence: 9,
                transfer_attempt: 2,
                generation: 3,
                updated_at_unix: 20,
            }),
        )
    );
}

#[tokio::test]
async fn test_copy_pass_restores_a_policy_retired_local_placement() {
    let source_dir = tempfile::tempdir().unwrap();
    let source_blobs = BlobStorage::filesystem(source_dir.path().join("blobs"));
    source_blobs.put_bytes(CONTENT).await.unwrap();
    let server = TestServer::start(
        crate::primary_router(
            "primary",
            "token",
            crate::support::distributed_meta(source_dir.path().join("peryx.redb")),
            source_blobs,
        )
        .unwrap(),
    )
    .await;
    let (_meta_dir, meta) = meta();
    let (_store_dir, store, backend) = filesystem();
    let (blob, artifact) = digests(CONTENT);
    store.write_verified(CONTENT, &blob).unwrap();
    let local = key(&artifact, &backend, "home", artifact.sha256());
    // `south` stays outside the target set, so its revocation is the control the refill must not touch.
    let south = key(&artifact, &backend, "south", "south/loc");
    seed_verified(&meta, &local, CONTENT.len() as u64);
    seed_verified(&meta, &south, CONTENT.len() as u64);
    seed_verified(
        &meta,
        &key(&artifact, &backend, "east", "peer/loc"),
        CONTENT.len() as u64,
    );
    let clock: Clock = Arc::new(|| 20);
    let retired = crate::FilesystemPlacementReconciler::new(dc("home"), store.clone(), BTreeSet::from([dc("east")]))
        .unwrap()
        .reconcile_pass(&meta, &clock, 5, &|| false, NonZeroUsize::MIN)
        .unwrap();
    let revoked = meta.blob_placement(&local).unwrap().unwrap();
    let copier = CrossDcBlobCopier::http(
        dc("home"),
        HashMap::from([("east".to_owned(), server.url.clone())]),
        "token",
        store.clone(),
        backend,
    )
    .unwrap()
    .unwrap();

    let refilled = copier
        .copy_pass(&meta, &clock, 9, &|| false, NonZeroUsize::MIN)
        .await
        .unwrap();

    assert_eq!(
        (
            retired,
            revoked.state,
            refilled,
            meta.blob_placement(&local).unwrap(),
            meta.blob_placement(&south).unwrap().map(|record| record.state),
            store.read(&blob).unwrap(),
        ),
        (
            AvailabilityTaskReport {
                processed: 2,
                changed: 2,
            },
            BlobPlacementState::Revoked,
            AvailabilityTaskReport {
                processed: 1,
                changed: 1,
            },
            Some(peryx_ha::BlobPlacementRecord {
                key: local,
                state: BlobPlacementState::Verified {
                    size: CONTENT.len() as u64,
                },
                fence: 9,
                transfer_attempt: 2,
                generation: 5,
                updated_at_unix: 20,
            }),
            Some(BlobPlacementState::Revoked),
            CONTENT.to_vec(),
        )
    );
}

#[tokio::test]
async fn test_concurrent_pass_does_not_duplicate_a_live_attempt() {
    let source_dir = tempfile::tempdir().unwrap();
    let source_blobs = BlobStorage::filesystem(source_dir.path().join("blobs"));
    source_blobs.put_bytes(CONTENT).await.unwrap();
    let (_meta_dir, meta) = meta();
    let (_store_dir, store, backend) = filesystem();
    let (_blob, artifact) = digests(CONTENT);
    seed_verified(
        &meta,
        &key(&artifact, &backend, "east", "peer/loc"),
        CONTENT.len() as u64,
    );
    let fetches = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let server = TestServer::start(
        crate::primary_router(
            "primary",
            "token",
            crate::support::distributed_meta(source_dir.path().join("peryx.redb")),
            source_blobs,
        )
        .unwrap()
        .layer(axum::middleware::from_fn_with_state(
            RequestGate {
                fetches: fetches.clone(),
                started: started.clone(),
                release: release.clone(),
            },
            gate_blob_request,
        )),
    )
    .await;
    let copier = Arc::new(
        CrossDcBlobCopier::http(
            dc("home"),
            HashMap::from([("east".to_owned(), server.url.clone())]),
            "token",
            store,
            backend,
        )
        .unwrap()
        .unwrap(),
    );
    let clock: Clock = Arc::new(|| 20);
    let first = tokio::spawn({
        let copier = copier.clone();
        let meta = meta.clone();
        let clock = clock.clone();
        async move {
            copier
                .copy_pass(&meta, &clock, 5, &|| false, NonZeroUsize::MIN)
                .await
                .unwrap()
        }
    });
    started.notified().await;

    let concurrent = copier
        .copy_pass(&meta, &clock, 5, &|| false, NonZeroUsize::MIN)
        .await
        .unwrap();
    release.notify_one();
    let first = first.await.unwrap();

    assert_eq!(
        (first, concurrent, fetches.load(Ordering::SeqCst)),
        (
            AvailabilityTaskReport {
                processed: 1,
                changed: 1,
            },
            AvailabilityTaskReport::default(),
            1,
        )
    );
}

#[tokio::test]
async fn test_copy_one_skips_an_unreachable_source() {
    let (_dir, meta) = meta();
    let (_store_dir, store, backend) = filesystem();
    let (_blob, artifact) = digests(CONTENT);
    let clock: Clock = Arc::new(|| 7);
    let copier = copier_with(
        "home",
        backend.clone(),
        store,
        Arc::new(FakePeers { peers: HashMap::new() }),
    );
    let copy = plan(&artifact, &backend, "east", "home", 4);
    let target = copy.target.clone();

    let recorded = copier.copy_one(&meta, &clock, copy).await;

    assert!(!recorded);
    assert!(
        meta.blob_placement(&target).unwrap().is_none(),
        "an unreachable source stages nothing"
    );
}

#[tokio::test]
async fn test_copy_one_stops_when_the_target_cannot_be_staged() {
    let (_dir, meta) = meta();
    let (_store_dir, store, backend) = filesystem();
    let (_blob2, artifact) = digests(CONTENT);
    for index in 0..peryx_ha::MAX_PLACEMENTS_PER_DIGEST {
        crate::apply_blob_placement(
            &meta,
            &key(&artifact, &backend, "east", &format!("loc-{index}")),
            &BlobPlacementTransition::Stage,
            5,
            0,
        )
        .unwrap();
    }
    let clock: Clock = Arc::new(|| 7);
    let copier = copier_with(
        "home",
        backend.clone(),
        store,
        Arc::new(FakePeers::holding("east", &Digest::of(CONTENT), CONTENT)),
    );

    let recorded = copier
        .copy_one(&meta, &clock, plan(&artifact, &backend, "east", "home", 4))
        .await;

    assert!(!recorded);
}

#[tokio::test]
async fn test_copy_one_records_a_source_loss() {
    let (_dir, meta) = meta();
    let (_store_dir, store, backend) = filesystem();
    let (_blob, artifact) = digests(CONTENT);
    let clock: Clock = Arc::new(|| 7);
    let copier = copier_with(
        "home",
        backend.clone(),
        store,
        Arc::new(FakePeers {
            peers: HashMap::from([(
                "east".to_owned(),
                LoopbackBlobSource::new(HashMap::new(), TransferLimits::default()),
            )]),
        }),
    );
    let copy = plan(&artifact, &backend, "east", "home", 4);
    let target = copy.target.clone();

    let recorded = copier.copy_one(&meta, &clock, copy).await;

    assert!(!recorded);
    assert_eq!(
        local_state(&meta, &target),
        BlobPlacementState::Failed {
            class: BlobPlacementFailure::SourceUnavailable
        }
    );
}

#[tokio::test]
async fn test_copy_one_records_a_local_byte_cap_apart_from_an_unreachable_source() {
    let (_dir, meta) = meta();
    let (_store_dir, store, backend) = filesystem();
    let (blob, artifact) = digests(CONTENT);
    let clock: Clock = Arc::new(|| 7);
    // A source cap below the planned range makes every attempt fail identically, whatever the peer does.
    let copier = copier_with(
        "home",
        backend.clone(),
        store,
        Arc::new(FakePeers {
            peers: HashMap::from([(
                "east".to_owned(),
                LoopbackBlobSource::new(
                    HashMap::from([(blob, Bytes::from_static(CONTENT))]),
                    TransferLimits {
                        max_encoded_bytes: NonZeroU64::new(4).unwrap(),
                        ..TransferLimits::default()
                    },
                ),
            )]),
        }),
    );
    let copy = plan(&artifact, &backend, "east", "home", CONTENT.len() as u64);
    let target = copy.target.clone();

    let recorded = copier.copy_one(&meta, &clock, copy).await;

    assert!(!recorded);
    assert_eq!(
        local_state(&meta, &target),
        BlobPlacementState::Failed {
            class: BlobPlacementFailure::TransferLimit
        }
    );
}

#[tokio::test]
async fn test_copy_one_records_a_digest_mismatch() {
    let (_dir, meta) = meta();
    let (_store_dir, store, backend) = filesystem();
    let (blob, artifact) = digests(CONTENT);
    let clock: Clock = Arc::new(|| 7);
    let copier = copier_with(
        "home",
        backend.clone(),
        store,
        Arc::new(FakePeers {
            peers: HashMap::from([(
                "east".to_owned(),
                LoopbackBlobSource::new(
                    HashMap::from([(blob, Bytes::from_static(b"different bytes entirely"))]),
                    TransferLimits::default(),
                ),
            )]),
        }),
    );
    let copy = plan(&artifact, &backend, "east", "home", 4);
    let target = copy.target.clone();

    let recorded = copier.copy_one(&meta, &clock, copy).await;

    assert!(!recorded);
    assert_eq!(
        local_state(&meta, &target),
        BlobPlacementState::Failed {
            class: BlobPlacementFailure::DigestMismatch
        }
    );
}

#[tokio::test]
async fn test_copy_one_records_a_publish_loss() {
    let (_dir, meta) = meta();
    let store_dir = tempfile::tempdir().unwrap();
    let root = store_dir.path().join("blocked");
    std::fs::write(&root, b"not a directory").unwrap();
    let store = BlobStore::new(&root);
    let backend = BackendId::new("filesystem").unwrap();
    let (blob, artifact) = digests(CONTENT);
    let clock: Clock = Arc::new(|| 7);
    let copier = copier_with(
        "home",
        backend.clone(),
        store,
        Arc::new(FakePeers::holding("east", &blob, CONTENT)),
    );
    let copy = plan(&artifact, &backend, "east", "home", CONTENT.len() as u64);
    let target = copy.target.clone();

    let recorded = copier.copy_one(&meta, &clock, copy).await;

    assert!(!recorded);
    assert_eq!(
        local_state(&meta, &target),
        BlobPlacementState::Failed {
            class: BlobPlacementFailure::BackendRejected
        }
    );
}

#[tokio::test]
async fn test_copy_pass_is_fenced_shut_without_a_cluster_term() {
    let (_meta_dir, meta) = meta();
    let (_store_dir, store, backend) = filesystem();
    let (blob, artifact) = digests(CONTENT);
    seed_verified(
        &meta,
        &key(&artifact, &backend, "east", "peer/loc"),
        CONTENT.len() as u64,
    );
    let copier = copier_with(
        "home",
        backend,
        store,
        Arc::new(FakePeers::holding("east", &blob, CONTENT)),
    );

    let clock: Clock = Arc::new(|| 42);
    let report = copier
        .copy_pass(&meta, &clock, 0, &|| false, NonZeroUsize::new(4).unwrap())
        .await
        .unwrap();

    assert_eq!(
        report,
        peryx_ha::AvailabilityTaskReport::default(),
        "no ownership term fences every copy shut"
    );
}

#[tokio::test]
async fn test_copy_pass_treats_an_absent_ledger_as_empty() {
    let meta_dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(meta_dir.path().join("peryx.redb")).unwrap();
    let (_store_dir, store, backend) = filesystem();
    let copier = copier_with("home", backend, store, Arc::new(FakePeers { peers: HashMap::new() }));
    let clock: Clock = Arc::new(|| 42);

    let report = copier
        .copy_pass(&meta, &clock, 1, &|| false, NonZeroUsize::new(4).unwrap())
        .await
        .unwrap();

    assert_eq!(report, peryx_ha::AvailabilityTaskReport::default());
}

#[tokio::test]
async fn test_copy_pass_drains_the_backlog_under_a_live_term() {
    let (_meta_dir, meta) = meta();
    let (_store_dir, store, backend) = filesystem();
    let (blob, artifact) = digests(CONTENT);
    seed_verified(
        &meta,
        &key(&artifact, &backend, "east", "peer/loc"),
        CONTENT.len() as u64,
    );
    let copier = copier_with(
        "home",
        backend.clone(),
        store.clone(),
        Arc::new(FakePeers::holding("east", &blob, CONTENT)),
    );

    let clock: Clock = Arc::new(|| 42);
    let report = copier
        .copy_pass(&meta, &clock, 9, &|| false, NonZeroUsize::new(4).unwrap())
        .await
        .unwrap();

    assert_eq!(
        report,
        AvailabilityTaskReport {
            processed: 1,
            changed: 1
        }
    );
    assert_eq!(store.read(&blob).unwrap(), CONTENT);
    let local = key(&artifact, &backend, "home", artifact.sha256());
    assert_eq!(local_state(&meta, &local).status(), BlobPlacementStatus::Verified);
}

#[tokio::test]
async fn test_http_copy_pass_reuses_one_connection_for_one_source() {
    let source_dir = tempfile::tempdir().unwrap();
    let source_blobs = BlobStorage::filesystem(source_dir.path().join("blobs"));
    let contents = [
        b"first cross-datacenter blob".as_slice(),
        b"second cross-datacenter blob".as_slice(),
    ];
    for content in contents {
        source_blobs.put_bytes(content).await.unwrap();
    }
    let router = crate::primary_router(
        "primary",
        "token",
        crate::support::distributed_meta(source_dir.path().join("peryx.redb")),
        source_blobs,
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let connections = Arc::new(AtomicUsize::new(0));
    let observed_connections = Arc::clone(&connections);
    let server = tokio::spawn(std::future::IntoFuture::into_future(axum::serve(
        listener.tap_io(move |_| {
            observed_connections.fetch_add(1, Ordering::Relaxed);
        }),
        router,
    )));
    let (_meta_dir, meta) = meta();
    let (_store_dir, store, backend) = filesystem();
    for content in contents {
        let (_, artifact) = digests(content);
        seed_verified(
            &meta,
            &key(&artifact, &backend, "east", "peer/loc"),
            content.len() as u64,
        );
    }
    let copier = CrossDcBlobCopier::http(
        dc("home"),
        HashMap::from([("east".to_owned(), format!("http://{address}/"))]),
        "token",
        store,
        backend,
    )
    .unwrap()
    .unwrap();
    let clock: Clock = Arc::new(|| 42);

    let report = copier
        .copy_pass(&meta, &clock, 9, &|| false, NonZeroUsize::MIN)
        .await
        .unwrap();
    server.abort();

    assert_eq!((report.changed, connections.load(Ordering::Relaxed)), (2, 1));
}

struct CountingSource {
    inner: LoopbackBlobSource,
    fetches: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl BlobTransport for CountingSource {
    async fn fetch_blob(&self, request: crate::BlobRequest) -> Result<Vec<u8>, TransportError> {
        self.fetches.fetch_add(1, Ordering::SeqCst);
        self.inner.fetch_blob(request).await
    }
}

struct CountingPeers {
    peer: CountingSource,
}

impl SourceTransports for CountingPeers {
    fn transport(&self, _source_dc: &str) -> Option<&(dyn BlobTransport + Send + Sync)> {
        Some(&self.peer as &(dyn BlobTransport + Send + Sync))
    }
}

fn counting_peers(owed: &[Owed]) -> (Arc<CountingPeers>, Arc<AtomicUsize>) {
    let fetches = Arc::new(AtomicUsize::new(0));
    let blobs = owed
        .iter()
        .map(|entry| (entry.blob.clone(), Bytes::copy_from_slice(entry.content)))
        .collect();
    let peers = Arc::new(CountingPeers {
        peer: CountingSource {
            inner: LoopbackBlobSource::new(blobs, TransferLimits::default()),
            fetches: fetches.clone(),
        },
    });
    (peers, fetches)
}

#[tokio::test]
async fn test_copy_pass_copies_before_the_scan_reaches_the_end_of_the_index() {
    let (_meta_dir, meta) = meta();
    let (_store_dir, store, backend) = filesystem();
    let owed = seed_owed(&meta, &backend, &[b"streamed-aa", b"streamed-bb", b"streamed-cc"]);
    let (peers, fetches) = counting_peers(&owed);
    let copier = copier_with("home", backend, store.clone(), peers);
    let clock: Clock = Arc::new(|| 42);
    let stop_after_first_fetch = || fetches.load(Ordering::SeqCst) > 0;

    let report = copier
        .paced_copy_pass(
            &meta,
            &clock,
            9,
            &stop_after_first_fetch,
            NonZeroUsize::MIN,
            pacing(1, 64),
        )
        .await
        .unwrap();

    assert_eq!(
        (report, meta.blob_copy_cursor("home").unwrap()),
        (
            AvailabilityTaskReport {
                processed: 1,
                changed: 1,
            },
            Some(owed[0].artifact.canonical()),
        ),
        "the first copy runs and the pass records where the scan stopped"
    );
    assert_eq!(store.read(&owed[0].blob).unwrap(), owed[0].content);
}

#[tokio::test]
async fn test_a_capped_pass_resumes_from_the_recorded_cursor() {
    let (_meta_dir, meta) = meta();
    let (_store_dir, store, backend) = filesystem();
    let owed = seed_owed(&meta, &backend, &[b"resumed-aa", b"resumed-bb"]);
    let (peers, _fetches) = counting_peers(&owed);
    let copier = copier_with("home", backend, store.clone(), peers);
    let clock: Clock = Arc::new(|| 42);

    let first = copier
        .paced_copy_pass(&meta, &clock, 9, &|| false, NonZeroUsize::MIN, pacing(1, 1))
        .await
        .unwrap();
    let recorded = meta.blob_copy_cursor("home").unwrap();
    let second = copier
        .paced_copy_pass(&meta, &clock, 9, &|| false, NonZeroUsize::MIN, pacing(1, 1))
        .await
        .unwrap();

    assert_eq!(
        (first, recorded, second, meta.blob_copy_cursor("home").unwrap()),
        (
            AvailabilityTaskReport {
                processed: 1,
                changed: 1,
            },
            Some(owed[0].artifact.canonical()),
            AvailabilityTaskReport {
                processed: 1,
                changed: 1,
            },
            None,
        ),
        "each pass copies one digest and the sweep clears its cursor at the end"
    );
    assert_eq!(store.read(&owed[1].blob).unwrap(), owed[1].content);
}

#[tokio::test]
async fn test_copy_pass_surfaces_a_scan_failure() {
    let (_dir, meta) = corrupt_placement_store();
    let (_store_dir, store, backend) = filesystem();
    let copier = copier_with("home", backend, store, Arc::new(FakePeers { peers: HashMap::new() }));
    let clock: Clock = Arc::new(|| 42);

    let error = copier
        .copy_pass(&meta, &clock, 9, &|| false, NonZeroUsize::MIN)
        .await
        .unwrap_err();

    assert_eq!(error.code(), "copy_backlog_scan");
}

#[tokio::test]
async fn test_copy_pass_reports_an_unreadable_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let database = redb::Database::create(&path).unwrap();
    let transaction = database.begin_write().unwrap();
    transaction
        .open_table(redb::TableDefinition::<&str, u64>::new("blob_copy_cursor"))
        .unwrap();
    transaction.commit().unwrap();
    drop(database);
    let meta = MetaStore::open_existing(path).unwrap();
    let (_store_dir, store, backend) = filesystem();
    let copier = copier_with("home", backend, store, Arc::new(FakePeers { peers: HashMap::new() }));
    let clock: Clock = Arc::new(|| 42);

    let error = copier
        .copy_pass(&meta, &clock, 9, &|| false, NonZeroUsize::MIN)
        .await
        .unwrap_err();

    assert_eq!(error.code(), "copy_cursor_read");
}

#[tokio::test]
async fn test_copy_pass_reports_an_unwritable_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let (_store_dir, store, backend) = filesystem();
    let seeded = crate::support::distributed_meta(&path);
    seed_owed(&seeded, &backend, &[b"unwritable-aa", b"unwritable-bb"]);
    drop(seeded);
    let meta = MetaStore::open_existing_read_only(path).unwrap();
    let copier = copier_with("home", backend, store, Arc::new(FakePeers { peers: HashMap::new() }));
    let clock: Clock = Arc::new(|| 42);

    let error = copier
        .paced_copy_pass(&meta, &clock, 9, &|| false, NonZeroUsize::MIN, pacing(1, 1))
        .await
        .unwrap_err();

    assert_eq!(error.code(), "copy_cursor_write");
}
