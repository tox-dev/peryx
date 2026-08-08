use std::num::NonZeroUsize;

use crate::LoopbackBlobSource;
use peryx_driver::state::{AppState, ClusterStatus, HomeClaim, OwnershipAuthority, OwnershipError, TransferOutcome};
use peryx_identity::ArtifactDigest;
use peryx_storage::blob::BlobStorage;
use peryx_storage::meta::{BlobPlacementState, BlobPlacementStatus, DataCenterId, VerifiedSource};
use tokio_util::bytes::Bytes;

use super::*;

const CONTENT: &[u8] = b"cross-datacenter artifact bytes";

#[test]
fn test_task_error_preserves_the_job_failure() {
    let error = task_error(JobFailure::new("copy_failed", "peer unavailable"));

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

    assert!(CrossDcBlobCopier::http(dc("home"), HashMap::new(), "token".to_owned(), store, backend).is_none());
}

#[test]
fn test_http_copier_accepts_a_remote_datacenter() {
    let (_dir, store, backend) = filesystem();

    assert!(
        CrossDcBlobCopier::http(
            dc("home"),
            HashMap::from([("east".to_owned(), "http://peer/".to_owned())]),
            "token".to_owned(),
            store,
            backend,
        )
        .is_some()
    );
}

fn meta() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
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
    meta.apply_blob_placement(key, &BlobPlacementTransition::Stage, 1, 10)
        .unwrap();
    meta.apply_blob_placement(
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

/// A source resolver over fixed in-process peers, so a pass copies without a socket.
struct FakePeers {
    peers: HashMap<String, HashMap<Digest, Bytes>>,
}

impl FakePeers {
    fn holding(data_center: &str, digest: &Digest, content: &[u8]) -> Self {
        Self {
            peers: HashMap::from([(
                data_center.to_owned(),
                HashMap::from([(digest.clone(), Bytes::copy_from_slice(content))]),
            )]),
        }
    }
}

impl SourceTransports for FakePeers {
    fn transport(&self, source_dc: &str) -> Option<Box<dyn BlobTransport + Send + Sync>> {
        let blobs = self.peers.get(source_dc)?;
        Some(Box::new(LoopbackBlobSource::new(
            blobs.clone(),
            TransferLimits::default(),
        )))
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
        fence: 5,
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
    // Fill the digest to its placement ceiling so staging one more key is rejected as over capacity.
    for index in 0..peryx_storage::meta::MAX_PLACEMENTS_PER_DIGEST {
        meta.apply_blob_placement(
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

#[test]
fn test_roster_transport_resolves_only_a_rostered_buildable_peer() {
    let good = RosterTransports {
        roster: HashMap::from([("east".to_owned(), "http://peer/".to_owned())]),
        token: "secret".to_owned(),
        limits: TransferLimits::default(),
    };
    assert!(good.transport("east").is_some());
    assert!(
        good.transport("absent").is_none(),
        "an unrostered datacenter resolves nothing"
    );

    let empty_token = RosterTransports {
        roster: HashMap::from([("east".to_owned(), "http://peer/".to_owned())]),
        token: String::new(),
        limits: TransferLimits::default(),
    };
    assert!(
        empty_token.transport("east").is_none(),
        "an address that cannot build a client resolves nothing"
    );
}

fn backlog_entry(digest: &ArtifactDigest, backend: &BackendId, source_dc: &str) -> CopyBacklogEntry {
    CopyBacklogEntry {
        digest: digest.clone(),
        sources: vec![VerifiedSource {
            key: key(digest, backend, source_dc, "peer/loc"),
            generation: 1,
            size: 4,
        }],
    }
}

#[test]
fn test_plan_one_resolves_a_backlog_entry_under_a_live_fence() {
    let (_dir, store, backend) = filesystem();
    let copier = copier_with(
        "home",
        backend.clone(),
        store,
        Arc::new(FakePeers { peers: HashMap::new() }),
    );
    let (_content, artifact) = digests(CONTENT);

    let copy = copier.plan_one(&backlog_entry(&artifact, &backend, "east"), 5).unwrap();

    assert_eq!(copy.target.data_center, dc("home"));
    assert_eq!(copy.source.data_center, dc("east"));
    assert_eq!(copy.fence, 5);
}

#[test]
fn test_plan_one_yields_nothing_when_fenced_or_sourceless() {
    let (_dir, store, backend) = filesystem();
    let copier = copier_with(
        "home",
        backend.clone(),
        store,
        Arc::new(FakePeers { peers: HashMap::new() }),
    );
    let (_content, artifact) = digests(CONTENT);

    assert!(
        copier
            .plan_one(&backlog_entry(&artifact, &backend, "east"), 0)
            .is_none()
    );
    assert!(
        copier
            .plan_one(
                &CopyBacklogEntry {
                    digest: artifact,
                    sources: Vec::new(),
                },
                5,
            )
            .is_none()
    );
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

    // A batch of one forces the pass to resume past its cursor for the second digest.
    let planned = copier.collect_backlog(&meta, 5, 1, &|| false).unwrap();

    assert_eq!(planned.len(), 2);
}

#[test]
fn test_collect_backlog_stops_when_cancelled() {
    let (_dir, meta) = meta();
    let (_store_dir, store, backend) = filesystem();
    let (_blob, artifact) = digests(CONTENT);
    seed_verified(&meta, &key(&artifact, &backend, "east", "peer/loc"), 4);
    let copier = copier_with("home", backend, store, Arc::new(FakePeers { peers: HashMap::new() }));

    let planned = copier.collect_backlog(&meta, 5, 256, &|| true).unwrap();

    assert!(planned.is_empty(), "a cancelled pass plans nothing");
}

#[test]
fn test_collect_backlog_surfaces_a_scan_rejection() {
    let (_dir, meta) = meta();
    let (_store_dir, store, backend) = filesystem();
    let copier = copier_with("home", backend, store, Arc::new(FakePeers { peers: HashMap::new() }));

    let error = copier.collect_backlog(&meta, 5, 0, &|| false).unwrap_err();

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

    let recorded = copier.copy_one(&meta, &clock, copy, 5).await;

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

    let recorded = copier.copy_one(&meta, &clock, copy, 5).await;

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
    for index in 0..peryx_storage::meta::MAX_PLACEMENTS_PER_DIGEST {
        meta.apply_blob_placement(
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
        .copy_one(&meta, &clock, plan(&artifact, &backend, "east", "home", 4), 5)
        .await;

    assert!(!recorded);
}

#[tokio::test]
async fn test_copy_one_records_a_source_loss() {
    let (_dir, meta) = meta();
    let (_store_dir, store, backend) = filesystem();
    let (_blob, artifact) = digests(CONTENT);
    let clock: Clock = Arc::new(|| 7);
    // The peer is reachable but serves no bytes for the digest.
    let copier = copier_with(
        "home",
        backend.clone(),
        store,
        Arc::new(FakePeers {
            peers: HashMap::from([("east".to_owned(), HashMap::new())]),
        }),
    );
    let copy = plan(&artifact, &backend, "east", "home", 4);
    let target = copy.target.clone();

    let recorded = copier.copy_one(&meta, &clock, copy, 5).await;

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
    // The peer serves bytes that do not hash to the claimed digest.
    let copier = copier_with(
        "home",
        backend.clone(),
        store,
        Arc::new(FakePeers {
            peers: HashMap::from([(
                "east".to_owned(),
                HashMap::from([(blob, Bytes::from_static(b"different bytes entirely"))]),
            )]),
        }),
    );
    let copy = plan(&artifact, &backend, "east", "home", 4);
    let target = copy.target.clone();

    let recorded = copier.copy_one(&meta, &clock, copy, 5).await;

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
    // A file where the store expects its root, so a verified publish cannot create the path.
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

    let recorded = copier.copy_one(&meta, &clock, copy, 5).await;

    assert!(!recorded);
    assert_eq!(
        local_state(&meta, &target),
        BlobPlacementState::Failed {
            class: BlobPlacementFailure::BackendRejected
        }
    );
}

struct FixedTerm(u64);

#[async_trait]
impl OwnershipAuthority for FixedTerm {
    async fn has_home(&self, _authority: &str) -> bool {
        true
    }

    async fn claim_home(&self, _authority: &str) -> Result<HomeClaim, OwnershipError> {
        Ok(HomeClaim::AlreadyHomed)
    }

    fn cluster_status(&self) -> ClusterStatus {
        ClusterStatus {
            leader: None,
            term: self.0,
            voters: Vec::new(),
        }
    }

    async fn committed_epoch(&self, _authority: &str) -> u64 {
        self.0
    }

    async fn admit_epoch(&self, _authority: &str, _presented: u64) -> bool {
        true
    }

    async fn transfer_home(
        &self,
        _authority: &str,
        _new_home: &str,
    ) -> Result<Option<TransferOutcome>, OwnershipError> {
        Ok(None)
    }
}

#[tokio::test]
async fn test_fixed_term_ownership_double_answers_its_snapshot() {
    let group = FixedTerm(7);
    assert_eq!(group.cluster_status().term, 7);
    assert!(group.has_home("a").await);
    assert_eq!(group.claim_home("a").await.unwrap(), HomeClaim::AlreadyHomed);
    assert_eq!(group.committed_epoch("a").await, 7);
    assert!(group.admit_epoch("a", 7).await);
}

fn app(meta: MetaStore) -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("state-blobs"));
    let state = AppState::new(meta, blobs, 60, Vec::new());
    (dir, Arc::new(state))
}

#[tokio::test]
async fn test_copy_pass_is_fenced_shut_without_a_cluster_term() {
    let (_meta_dir, meta) = meta();
    let (_store_dir, store, backend) = filesystem();
    let (blob, artifact) = digests(CONTENT);
    let (_app_dir, state) = app(meta);
    seed_verified(
        &state.meta,
        &key(&artifact, &backend, "east", "peer/loc"),
        CONTENT.len() as u64,
    );
    let copier = copier_with(
        "home",
        backend,
        store,
        Arc::new(FakePeers::holding("east", &blob, CONTENT)),
    );

    let report = copier
        .bind(state.serving.clone())
        .copy_pass(&|| false, NonZeroUsize::new(4).unwrap())
        .await
        .unwrap();

    assert_eq!(
        report,
        peryx_ha::AvailabilityTaskReport::default(),
        "no ownership term fences every copy shut"
    );
}

#[tokio::test]
async fn test_copy_pass_drains_the_backlog_under_a_live_term() {
    let (_meta_dir, meta) = meta();
    let (_store_dir, store, backend) = filesystem();
    let (blob, artifact) = digests(CONTENT);
    let (_app_dir, state) = app(meta);
    seed_verified(
        &state.meta,
        &key(&artifact, &backend, "east", "peer/loc"),
        CONTENT.len() as u64,
    );
    state.set_ownership_authority(Arc::new(FixedTerm(9)));
    let copier = copier_with(
        "home",
        backend.clone(),
        store.clone(),
        Arc::new(FakePeers::holding("east", &blob, CONTENT)),
    );

    let report = copier
        .copy_pass(&state, &|| false, DcCopyParameters::new())
        .await
        .unwrap();

    assert_eq!(
        report,
        JobReport {
            processed: 1,
            changed: 1
        }
    );
    assert_eq!(store.read(&blob).unwrap(), CONTENT);
    let local = key(&artifact, &backend, "home", artifact.sha256());
    assert_eq!(local_state(&state.meta, &local).status(), BlobPlacementStatus::Verified);
}
