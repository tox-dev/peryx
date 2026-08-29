use std::num::{NonZeroU64, NonZeroUsize};
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::LoopbackBlobSource;
use axum::serve::ListenerExt as _;
use peryx_ha::{BackendLocation, BlobPlacementState, BlobPlacementStatus, DataCenterId};
use peryx_identity::ArtifactDigest;
use peryx_storage::blob::BlobStorage;
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

    assert!(recorded);
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

    assert!(!recorded);
}

#[test]
fn test_failure_class_maps_each_loss_to_its_evidence() {
    assert_eq!(
        failure_class(&CopyError::Fetch(TransportError::DigestMismatch {
            expected: "a".to_owned(),
            actual: "b".to_owned(),
        })),
        BlobPlacementFailure::DigestMismatch
    );
    assert_eq!(
        failure_class(&CopyError::Fetch(TransportError::BlobNotFound {
            digest: "x".to_owned(),
        })),
        BlobPlacementFailure::SourceUnavailable
    );
    assert_eq!(
        failure_class(&CopyError::Publish(peryx_storage::blob::BlobError::unsupported("no"))),
        BlobPlacementFailure::BackendRejected
    );
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

#[test]
fn test_collect_backlog_plans_every_owed_digest_across_pages() {
    let (_dir, meta) = meta();
    let (_store_dir, store, backend) = filesystem();
    for suffix in ["aa", "bb"] {
        let content = format!("blob-{suffix}");
        let (_blob, artifact) = digests(content.as_bytes());
        seed_verified(&meta, &key(&artifact, &backend, "east", "peer/loc"), 4);
    }
    let copier = copier_with("home", backend, store, Arc::new(FakePeers { peers: HashMap::new() }));

    let planned = copier
        .collect_backlog(
            &meta,
            NonZeroU64::new(5).unwrap(),
            NonZeroUsize::new(1).unwrap(),
            &|| false,
        )
        .unwrap();

    assert_eq!(planned.len(), 2);
}

#[test]
fn test_collect_backlog_stops_when_cancelled() {
    let (_dir, meta) = meta();
    let (_store_dir, store, backend) = filesystem();
    let (_blob, artifact) = digests(CONTENT);
    seed_verified(&meta, &key(&artifact, &backend, "east", "peer/loc"), 4);
    let copier = copier_with("home", backend, store, Arc::new(FakePeers { peers: HashMap::new() }));

    let planned = copier
        .collect_backlog(
            &meta,
            NonZeroU64::new(5).unwrap(),
            NonZeroUsize::new(256).unwrap(),
            &|| true,
        )
        .unwrap();

    assert!(planned.is_empty(), "a cancelled pass plans nothing");
}

#[test]
fn test_collect_backlog_surfaces_a_scan_failure() {
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
    let meta = MetaStore::open_existing(path).unwrap();
    let (_store_dir, store, backend) = filesystem();
    let copier = copier_with("home", backend, store, Arc::new(FakePeers { peers: HashMap::new() }));

    let error = copier
        .collect_backlog(
            &meta,
            NonZeroU64::new(5).unwrap(),
            NonZeroUsize::new(1).unwrap(),
            &|| false,
        )
        .unwrap_err();

    assert_eq!(error.code(), "copy_backlog_scan");
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
