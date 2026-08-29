use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};

use peryx_ha::{
    BackendId, BackendLocation, BlobPlacementKey, BlobPlacementTransition, DataCenterId, ReclamationState,
    ReclamationStatus, ReclamationStore,
};
use peryx_identity::ArtifactDigest;
use peryx_storage::blob::{BlobStorage, Digest};
use peryx_storage::meta::MetaStore;
use redb::{Database, TableDefinition};

use super::*;
use crate::{HeartbeatReport, LivenessTracker};

fn meta() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::support::distributed_meta(dir.path().join("peryx.redb"));
    (dir, store)
}

struct Runtime {
    meta: MetaStore,
    blobs: BlobStorage,
    clock: Clock,
}

fn app(meta: MetaStore) -> (tempfile::TempDir, Runtime) {
    let dir = tempfile::tempdir().unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    (
        dir,
        Runtime {
            meta,
            blobs,
            clock: Arc::new(|| 42),
        },
    )
}

fn store_blob(state: &Runtime, content: &[u8]) -> (Digest, ArtifactDigest) {
    let blob = Digest::of(content);
    state
        .blobs
        .filesystem_store()
        .unwrap()
        .write_verified(content, &blob)
        .unwrap();
    let artifact = ArtifactDigest::from_sha256(blob.as_str()).unwrap();
    (blob, artifact)
}

fn batch() -> NonZeroUsize {
    NonZeroUsize::new(256).unwrap()
}

fn advance_serial(meta: &MetaStore, serial: u64) {
    for _ in 0..serial {
        meta.next_serial().unwrap();
    }
}

fn verified_placement(meta: &MetaStore, digest: &ArtifactDigest) {
    let key = BlobPlacementKey {
        digest: digest.clone(),
        backend: BackendId::new("filesystem").unwrap(),
        data_center: DataCenterId::new("home").unwrap(),
        location: BackendLocation::new("home/loc").unwrap(),
    };
    crate::apply_blob_placement(meta, &key, &BlobPlacementTransition::Stage, 1, 10).unwrap();
    crate::apply_blob_placement(
        meta,
        &key,
        &BlobPlacementTransition::Verify {
            attempt: 1,
            observed: digest.clone(),
            size: 1,
        },
        1,
        11,
    )
    .unwrap();
}

struct StubRefs(BTreeSet<String>);

impl ReferenceInventory for StubRefs {
    fn referenced(&self) -> Result<BTreeSet<String>, String> {
        Ok(self.0.clone())
    }
}

struct FailingRefs;

impl ReferenceInventory for FailingRefs {
    fn referenced(&self) -> Result<BTreeSet<String>, String> {
        Err("reference scan failed".to_owned())
    }
}

fn selector(referenced: &[&ArtifactDigest], frontier: ObservedFrontier) -> BlobReclamationSelector {
    let set = referenced.iter().map(|digest| digest.sha256().to_owned()).collect();
    BlobReclamationSelector::new(Arc::new(StubRefs(set)), Arc::new(StubFrontiers(frontier)))
}

struct StubFrontiers(ObservedFrontier);

impl ReclamationFrontiers for StubFrontiers {
    fn observe(&self) -> Option<ObservedFrontier> {
        Some(self.0)
    }
}

fn tombstone_status(meta: &MetaStore, digest: &ArtifactDigest) -> Option<ReclamationStatus> {
    meta.reclamation_tombstone(digest)
        .unwrap()
        .map(|tombstone| tombstone.state.status())
}

#[tokio::test]
async fn test_reclaim_pass_is_a_no_op_without_a_cluster_term() {
    let (_meta_dir, meta) = meta();
    let (_app_dir, state) = app(meta);
    let (_blob, artifact) = store_blob(&state, b"orphan");

    let report = selector(
        &[],
        ObservedFrontier {
            replica: Some(9),
            backup: Some(9),
        },
    )
    .bind(state.meta.clone(), state.blobs.clone(), state.clock.clone())
    .reclaim_pass(&|| false, 0, std::num::NonZeroUsize::new(100).unwrap())
    .await
    .unwrap();

    assert_eq!(
        report,
        peryx_ha::AvailabilityTaskReport::default(),
        "term 0 fences the pass shut"
    );
    assert_eq!(tombstone_status(&state.meta, &artifact), None);
}

#[tokio::test]
async fn test_pass_selects_an_unreferenced_digest_and_stamps_the_frontier() {
    let (_meta_dir, meta) = meta();
    advance_serial(&meta, 5);
    let (_app_dir, state) = app(meta);
    let (_blob, artifact) = store_blob(&state, b"orphan");

    let report = selector(
        &[],
        ObservedFrontier {
            replica: Some(0),
            backup: Some(0),
        },
    )
    .reclaim_pass(&state.meta, &state.blobs, &state.clock, &|| false, 9, batch())
    .unwrap();

    assert_eq!(report.processed, 1);
    let tombstone = state.meta.reclamation_tombstone(&artifact).unwrap().unwrap();
    assert_eq!(tombstone.state, ReclamationState::Pending);
    assert_eq!(
        tombstone.required_frontier, 5,
        "the current metadata serial gates deletion"
    );
    assert_eq!(tombstone.fence, 9);
}

#[tokio::test]
async fn test_pass_leaves_a_referenced_digest_untouched() {
    let (_meta_dir, meta) = meta();
    let (_app_dir, state) = app(meta);
    let (_blob, artifact) = store_blob(&state, b"kept");

    let report = selector(
        &[&artifact],
        ObservedFrontier {
            replica: Some(9),
            backup: Some(9),
        },
    )
    .reclaim_pass(&state.meta, &state.blobs, &state.clock, &|| false, 9, batch())
    .unwrap();

    assert_eq!(report.processed, 1);
    assert_eq!(report.changed, 0);
    assert_eq!(
        tombstone_status(&state.meta, &artifact),
        None,
        "a referenced digest is never selected"
    );
}

#[tokio::test]
async fn test_pass_leaves_a_serveable_digest_untouched() {
    let (_meta_dir, meta) = meta();
    let (_app_dir, state) = app(meta);
    let (_blob, artifact) = store_blob(&state, b"serveable");
    verified_placement(&state.meta, &artifact);

    let report = selector(
        &[],
        ObservedFrontier {
            replica: Some(9),
            backup: Some(9),
        },
    )
    .reclaim_pass(&state.meta, &state.blobs, &state.clock, &|| false, 9, batch())
    .unwrap();

    assert_eq!(report.changed, 0);
    assert_eq!(
        tombstone_status(&state.meta, &artifact),
        None,
        "a verified placement a replica can serve blocks selection"
    );
}

#[tokio::test]
async fn test_pass_scans_only_the_batch() {
    let (_meta_dir, meta) = meta();
    let (_app_dir, state) = app(meta);
    store_blob(&state, b"one");
    store_blob(&state, b"two");
    let batch = NonZeroUsize::MIN;

    let report = selector(
        &[],
        ObservedFrontier {
            replica: Some(0),
            backup: Some(0),
        },
    )
    .reclaim_pass(&state.meta, &state.blobs, &state.clock, &|| false, 9, batch)
    .unwrap();

    assert_eq!(report.processed, 1, "the batch bounds one pass to a single candidate");
}

#[tokio::test]
async fn test_a_covered_frontier_marks_a_candidate_ready() {
    let (_meta_dir, meta) = meta();
    advance_serial(&meta, 4);
    let (_app_dir, state) = app(meta);
    let (_blob, artifact) = store_blob(&state, b"orphan");

    let report = selector(
        &[],
        ObservedFrontier {
            replica: Some(4),
            backup: Some(6),
        },
    )
    .reclaim_pass(&state.meta, &state.blobs, &state.clock, &|| false, 9, batch())
    .unwrap();

    assert_eq!(tombstone_status(&state.meta, &artifact), Some(ReclamationStatus::Ready));
    assert_eq!(report.changed, 2, "one selection and one readiness advance");
}

#[tokio::test]
async fn test_a_lagging_replica_blocks_readiness() {
    let (_meta_dir, meta) = meta();
    advance_serial(&meta, 5);
    let (_app_dir, state) = app(meta);
    let (_blob, artifact) = store_blob(&state, b"orphan");

    selector(
        &[],
        ObservedFrontier {
            replica: Some(2),
            backup: Some(9),
        },
    )
    .reclaim_pass(&state.meta, &state.blobs, &state.clock, &|| false, 9, batch())
    .unwrap();

    assert_eq!(
        tombstone_status(&state.meta, &artifact),
        Some(ReclamationStatus::Pending),
        "a replica short of the required frontier keeps the candidate pending"
    );
}

#[tokio::test]
async fn test_a_lagging_backup_blocks_readiness() {
    let (_meta_dir, meta) = meta();
    advance_serial(&meta, 5);
    let (_app_dir, state) = app(meta);
    let (_blob, artifact) = store_blob(&state, b"orphan");

    selector(
        &[],
        ObservedFrontier {
            replica: Some(9),
            backup: Some(2),
        },
    )
    .reclaim_pass(&state.meta, &state.blobs, &state.clock, &|| false, 9, batch())
    .unwrap();

    assert_eq!(
        tombstone_status(&state.meta, &artifact),
        Some(ReclamationStatus::Pending),
        "a backup short of the required frontier keeps the candidate pending"
    );
}

#[tokio::test]
async fn test_a_reference_returning_before_the_final_check_skips_the_candidate() {
    let (_meta_dir, meta) = meta();
    advance_serial(&meta, 3);
    let (_app_dir, state) = app(meta);
    let (_blob, artifact) = store_blob(&state, b"orphan");
    selector(
        &[],
        ObservedFrontier {
            replica: Some(0),
            backup: Some(0),
        },
    )
    .reclaim_pass(&state.meta, &state.blobs, &state.clock, &|| false, 9, batch())
    .unwrap();
    assert_eq!(
        tombstone_status(&state.meta, &artifact),
        Some(ReclamationStatus::Pending)
    );

    selector(
        &[&artifact],
        ObservedFrontier {
            replica: Some(9),
            backup: Some(9),
        },
    )
    .reclaim_pass(&state.meta, &state.blobs, &state.clock, &|| false, 9, batch())
    .unwrap();

    assert_eq!(
        tombstone_status(&state.meta, &artifact),
        Some(ReclamationStatus::Skipped),
        "a reference that returned skips the candidate rather than marking it ready"
    );
}

#[tokio::test]
async fn test_a_stale_worker_is_fenced_out_of_selection() {
    let (_meta_dir, meta) = meta();
    let (_app_dir, state) = app(meta);
    let (_blob, artifact) = store_blob(&state, b"contested");
    select_reclamation_candidate(&state.meta, &artifact, false, 0, 12, 10).unwrap();

    let error = selector(
        &[],
        ObservedFrontier {
            replica: Some(9),
            backup: Some(9),
        },
    )
    .reclaim_pass(&state.meta, &state.blobs, &state.clock, &|| false, 5, batch())
    .unwrap_err();

    assert_eq!(error.code(), "reclamation_select");
    assert_eq!(
        state.meta.reclamation_tombstone(&artifact).unwrap().unwrap().fence,
        12,
        "the superseded worker leaves the newer holder's tombstone untouched"
    );
}

#[tokio::test]
async fn test_a_cancelled_pass_selects_nothing() {
    let (_meta_dir, meta) = meta();
    let (_app_dir, state) = app(meta);
    let (_blob, artifact) = store_blob(&state, b"orphan");

    let report = selector(
        &[],
        ObservedFrontier {
            replica: Some(9),
            backup: Some(9),
        },
    )
    .reclaim_pass(&state.meta, &state.blobs, &state.clock, &|| true, 9, batch())
    .unwrap();

    assert_eq!(report, AvailabilityTaskReport::default());
    assert_eq!(tombstone_status(&state.meta, &artifact), None);
}

#[tokio::test]
async fn test_a_reference_scan_failure_surfaces() {
    let (_meta_dir, meta) = meta();
    let (_app_dir, state) = app(meta);
    store_blob(&state, b"orphan");
    let reclaimer = BlobReclamationSelector {
        references: Arc::new(FailingRefs),
        frontiers: Arc::new(StubFrontiers(ObservedFrontier {
            replica: Some(0),
            backup: Some(0),
        })),
    };

    let error = reclaimer
        .bind(state.meta.clone(), state.blobs.clone(), state.clock.clone())
        .reclaim_pass(&|| false, 9, std::num::NonZeroUsize::new(100).unwrap())
        .await
        .unwrap_err();

    assert_eq!(error.code(), "reclamation_references");
}

#[test]
fn test_reclaim_pass_surfaces_a_frontier_read_failure() {
    let dir = tempfile::tempdir().unwrap();
    let database = Database::create(dir.path().join("peryx.redb")).unwrap();
    let write = database.begin_write().unwrap();
    write.open_table(TableDefinition::<&str, &str>::new("serial")).unwrap();
    write.commit().unwrap();
    drop(database);
    let (_app_dir, state) = app(MetaStore::open_existing(dir.path().join("peryx.redb")).unwrap());

    let error = selector(
        &[],
        ObservedFrontier {
            replica: Some(0),
            backup: Some(0),
        },
    )
    .reclaim_pass(&state.meta, &state.blobs, &state.clock, &|| false, 9, batch())
    .unwrap_err();

    assert_eq!(error.code(), "reclamation_frontier_read");
}

#[test]
fn test_reclaim_pass_surfaces_a_blob_scan_failure() {
    let (_meta_dir, meta) = meta();
    let dir = tempfile::tempdir().unwrap();
    let blob_path = dir.path().join("blobs");
    std::fs::create_dir(&blob_path).unwrap();
    std::fs::write(blob_path.join("sha256"), b"not a directory").unwrap();
    let state = Runtime {
        meta,
        blobs: BlobStorage::filesystem(blob_path),
        clock: Arc::new(|| 42),
    };

    let error = selector(
        &[],
        ObservedFrontier {
            replica: Some(0),
            backup: Some(0),
        },
    )
    .reclaim_pass(&state.meta, &state.blobs, &state.clock, &|| false, 9, batch())
    .unwrap_err();

    assert_eq!(error.code(), "reclamation_scan");
}

#[test]
fn test_reclaim_pass_surfaces_a_tombstone_read_failure() {
    let dir = tempfile::tempdir().unwrap();
    let database = Database::create(dir.path().join("peryx.redb")).unwrap();
    let write = database.begin_write().unwrap();
    write.open_table(TableDefinition::<&str, u64>::new("serial")).unwrap();
    write
        .open_table(TableDefinition::<&str, u64>::new("reclamation_tombstone"))
        .unwrap();
    write.commit().unwrap();
    drop(database);
    let (_app_dir, state) = app(MetaStore::open_existing(dir.path().join("peryx.redb")).unwrap());

    let error = selector(
        &[],
        ObservedFrontier {
            replica: Some(0),
            backup: Some(0),
        },
    )
    .reclaim_pass(&state.meta, &state.blobs, &state.clock, &|| false, 9, batch())
    .unwrap_err();

    assert_eq!(error.code(), "reclamation_read");
}

struct MissingFrontiers;

impl ReclamationFrontiers for MissingFrontiers {
    fn observe(&self) -> Option<ObservedFrontier> {
        None
    }
}

#[test]
fn test_reclaim_pass_keeps_pending_state_without_frontier_evidence() {
    let (_meta_dir, meta) = meta();
    let artifact = ArtifactDigest::from_sha256("d".repeat(64)).unwrap();
    select_reclamation_candidate(&meta, &artifact, false, 0, 9, 10).unwrap();
    let (_app_dir, state) = app(meta);
    let reclaimer = BlobReclamationSelector::new(Arc::new(StubRefs(BTreeSet::new())), Arc::new(MissingFrontiers));

    let report = reclaimer
        .reclaim_pass(&state.meta, &state.blobs, &state.clock, &|| false, 9, batch())
        .unwrap();

    assert_eq!(report, AvailabilityTaskReport::default());
    assert_eq!(
        tombstone_status(&state.meta, &artifact),
        Some(ReclamationStatus::Pending)
    );
}

#[test]
fn test_replica_frontiers_impose_no_requirement_without_replicas() {
    assert_eq!(
        ReplicaReclamationFrontiers::new(None, Vec::new()).observe(),
        Some(ObservedFrontier {
            replica: None,
            backup: None,
        })
    );
}

#[test]
fn test_replica_frontiers_require_liveness_for_configured_replicas() {
    assert_eq!(
        ReplicaReclamationFrontiers::new(None, vec!["replica".to_owned()]).observe(),
        None
    );
}

#[test]
fn test_replica_frontiers_use_the_lowest_reported_serial() {
    let tracker = Arc::new(LivenessTracker::new(
        ["first".to_owned(), "second".to_owned()],
        Duration::from_secs(30),
        Duration::from_mins(1),
    ));
    let now = Instant::now();
    for (node, applied) in [("first", 8), ("second", 5)] {
        tracker
            .observe(
                &HeartbeatReport {
                    node: node.to_owned(),
                    incarnation: 1,
                    sequence: 1,
                    applied: Some(applied),
                },
                now,
            )
            .unwrap();
    }

    assert_eq!(
        ReplicaReclamationFrontiers::new(Some(tracker), vec!["first".to_owned(), "second".to_owned()]).observe(),
        Some(ObservedFrontier {
            replica: Some(5),
            backup: None,
        })
    );
}

#[test]
fn test_reclaim_pass_stops_before_finalizing_when_cancelled() {
    let (_meta_dir, meta) = meta();
    let artifact = ArtifactDigest::from_sha256("a".repeat(64)).unwrap();
    select_reclamation_candidate(&meta, &artifact, false, 0, 9, 10).unwrap();
    let (_app_dir, state) = app(meta);

    let report = selector(
        &[],
        ObservedFrontier {
            replica: Some(9),
            backup: Some(9),
        },
    )
    .reclaim_pass(&state.meta, &state.blobs, &state.clock, &|| true, 9, batch())
    .unwrap();

    assert_eq!(report, AvailabilityTaskReport::default());
    assert_eq!(
        tombstone_status(&state.meta, &artifact),
        Some(ReclamationStatus::Pending)
    );
}

#[test]
fn test_reclaim_pass_ignores_a_finalized_tombstone() {
    let (_meta_dir, meta) = meta();
    let artifact = ArtifactDigest::from_sha256("b".repeat(64)).unwrap();
    select_reclamation_candidate(&meta, &artifact, false, 0, 9, 10).unwrap();
    mark_reclamation_ready(
        &meta,
        &artifact,
        false,
        ObservedFrontier {
            replica: Some(0),
            backup: Some(0),
        },
        9,
        11,
    )
    .unwrap();
    let (_app_dir, state) = app(meta);

    let report = selector(
        &[],
        ObservedFrontier {
            replica: Some(9),
            backup: Some(9),
        },
    )
    .reclaim_pass(&state.meta, &state.blobs, &state.clock, &|| false, 9, batch())
    .unwrap();

    assert_eq!(report, AvailabilityTaskReport::default());
    assert_eq!(tombstone_status(&state.meta, &artifact), Some(ReclamationStatus::Ready));
}

#[test]
fn test_reclaim_pass_surfaces_a_stale_finalization_fence() {
    let (_meta_dir, meta) = meta();
    let artifact = ArtifactDigest::from_sha256("c".repeat(64)).unwrap();
    select_reclamation_candidate(&meta, &artifact, false, 0, 12, 10).unwrap();
    let (_app_dir, state) = app(meta);

    let error = selector(
        &[],
        ObservedFrontier {
            replica: Some(9),
            backup: Some(9),
        },
    )
    .reclaim_pass(&state.meta, &state.blobs, &state.clock, &|| false, 5, batch())
    .unwrap_err();

    assert_eq!(error.code(), "reclamation_mark");
}

#[test]
fn test_reclamation_policy_forgets_only_the_current_fence() {
    let (_meta_dir, meta) = meta();
    let artifact = ArtifactDigest::from_sha256("e".repeat(64)).unwrap();
    assert!(!forget_reclamation_tombstone(&meta, &artifact, 1).unwrap());
    select_reclamation_candidate(&meta, &artifact, false, 0, 5, 10).unwrap();
    assert!(matches!(
        forget_reclamation_tombstone(&meta, &artifact, 3).unwrap_err(),
        ReclamationError::Decision(peryx_ha::ReclamationDecisionError::StaleFence { current: 5, applied: 3 })
    ));
    assert!(forget_reclamation_tombstone(&meta, &artifact, 5).unwrap());
    assert!(meta.reclamation_tombstone(&artifact).unwrap().is_none());
}

#[test]
fn concurrent_reclamation_updates_converge() {
    let (_meta_dir, meta) = meta();
    let observed = ObservedFrontier {
        replica: Some(0),
        backup: Some(0),
    };
    for round in 0..16 {
        let artifact = ArtifactDigest::from_sha256(format!("{round:064x}")).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(16));
        std::thread::scope(|scope| {
            let updates = (0..16)
                .map(|_| {
                    let artifact = artifact.clone();
                    let barrier = barrier.clone();
                    let meta = meta.clone();
                    scope.spawn(move || {
                        barrier.wait();
                        select_reclamation_candidate(&meta, &artifact, false, 0, 9, 10)
                    })
                })
                .collect::<Vec<_>>();
            for update in updates {
                update.join().unwrap().unwrap();
            }
        });
        let barrier = Arc::new(std::sync::Barrier::new(16));
        std::thread::scope(|scope| {
            let updates = (0..16)
                .map(|_| {
                    let artifact = artifact.clone();
                    let barrier = barrier.clone();
                    let meta = meta.clone();
                    scope.spawn(move || {
                        barrier.wait();
                        mark_reclamation_ready(&meta, &artifact, false, observed, 9, 11)
                    })
                })
                .collect::<Vec<_>>();
            for update in updates {
                update.join().unwrap().unwrap();
            }
        });
        let barrier = Arc::new(std::sync::Barrier::new(16));
        std::thread::scope(|scope| {
            let removals: [_; 16] = std::array::from_fn(|_| {
                let artifact = artifact.clone();
                let barrier = barrier.clone();
                let meta = meta.clone();
                scope.spawn(move || {
                    barrier.wait();
                    forget_reclamation_tombstone(&meta, &artifact, 9).unwrap()
                })
            });
            assert_eq!(
                removals
                    .into_iter()
                    .map(|removal| removal.join().unwrap())
                    .filter(|removed| *removed)
                    .count(),
                1
            );
        });
        assert!(meta.reclamation_tombstone(&artifact).unwrap().is_none());
    }
}

#[test]
fn test_reclamation_policy_reports_missing_and_terminal_readiness() {
    let (_meta_dir, meta) = meta();
    let artifact = ArtifactDigest::from_sha256("f".repeat(64)).unwrap();
    let observed = ObservedFrontier {
        replica: Some(0),
        backup: Some(0),
    };
    assert!(matches!(
        mark_reclamation_ready(&meta, &artifact, false, observed, 1, 10).unwrap_err(),
        ReclamationError::Decision(peryx_ha::ReclamationDecisionError::MissingCandidate)
    ));
    select_reclamation_candidate(&meta, &artifact, false, 0, 1, 10).unwrap();
    let first = mark_reclamation_ready(&meta, &artifact, false, observed, 1, 11).unwrap();
    assert_eq!(
        mark_reclamation_ready(&meta, &artifact, false, observed, 1, 12).unwrap(),
        first
    );
}

#[test]
fn test_reclamation_retention_counts_and_prunes_only_skipped_records() {
    let (_meta_dir, meta) = meta();
    let pending = ArtifactDigest::from_sha256("1".repeat(64)).unwrap();
    let ready = ArtifactDigest::from_sha256("2".repeat(64)).unwrap();
    let skipped_one = ArtifactDigest::from_sha256("3".repeat(64)).unwrap();
    let skipped_two = ArtifactDigest::from_sha256("4".repeat(64)).unwrap();
    let observed = ObservedFrontier {
        replica: Some(0),
        backup: Some(0),
    };
    for digest in [&pending, &ready, &skipped_one, &skipped_two] {
        select_reclamation_candidate(&meta, digest, false, 0, 1, 10).unwrap();
    }
    mark_reclamation_ready(&meta, &ready, false, observed, 1, 11).unwrap();
    mark_reclamation_ready(&meta, &skipped_one, true, observed, 1, 11).unwrap();
    mark_reclamation_ready(&meta, &skipped_two, true, observed, 1, 11).unwrap();

    assert_eq!(
        reclamation_progress(&meta).unwrap(),
        peryx_ha::ReclamationProgress {
            pending: 1,
            ready: 1,
            skipped: 2,
        }
    );
    assert_eq!(prune_skipped_reclamation_tombstones(&meta, 0).unwrap(), 0);
    assert_eq!(prune_skipped_reclamation_tombstones(&meta, 1).unwrap(), 1);
    assert_eq!(prune_skipped_reclamation_tombstones(&meta, 8).unwrap(), 1);
    assert_eq!(
        reclamation_progress(&meta).unwrap(),
        peryx_ha::ReclamationProgress {
            pending: 1,
            ready: 1,
            skipped: 0,
        }
    );
}
