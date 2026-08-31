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
    select_reclamation_candidate(&state.meta, &artifact, false, 0, 0, 12, 10).unwrap();

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
        .open_table(TableDefinition::<&str, u64>::new("reference_revision"))
        .unwrap();
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
    select_reclamation_candidate(&meta, &artifact, false, 0, 0, 9, 10).unwrap();
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
    select_reclamation_candidate(&meta, &artifact, false, 0, 0, 9, 10).unwrap();
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
    select_reclamation_candidate(&meta, &artifact, false, 0, 0, 9, 10).unwrap();
    mark_reclamation_ready(
        &meta,
        &artifact,
        false,
        0,
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
    select_reclamation_candidate(&meta, &artifact, false, 0, 0, 12, 10).unwrap();
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
    select_reclamation_candidate(&meta, &artifact, false, 0, 0, 5, 10).unwrap();
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
                        select_reclamation_candidate(&meta, &artifact, false, 0, 0, 9, 10)
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
                        mark_reclamation_ready(&meta, &artifact, false, 0, observed, 9, 11)
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
        mark_reclamation_ready(&meta, &artifact, false, 0, observed, 1, 10).unwrap_err(),
        ReclamationError::Decision(peryx_ha::ReclamationDecisionError::MissingCandidate)
    ));
    select_reclamation_candidate(&meta, &artifact, false, 0, 0, 1, 10).unwrap();
    let first = mark_reclamation_ready(&meta, &artifact, false, 0, observed, 1, 11).unwrap();
    assert_eq!(
        mark_reclamation_ready(&meta, &artifact, false, 0, observed, 1, 12).unwrap(),
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
        select_reclamation_candidate(&meta, digest, false, 0, 0, 1, 10).unwrap();
    }
    mark_reclamation_ready(&meta, &ready, false, 0, observed, 1, 11).unwrap();
    mark_reclamation_ready(&meta, &skipped_one, true, 0, observed, 1, 11).unwrap();
    mark_reclamation_ready(&meta, &skipped_two, true, 0, observed, 1, 11).unwrap();

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

fn covered() -> ObservedFrontier {
    ObservedFrontier {
        replica: Some(0),
        backup: Some(0),
    }
}

/// Digests follow from blob content, so the caller learns the scan order rather than choosing it.
fn ordered_blobs(state: &Runtime, count: usize) -> Vec<(Digest, ArtifactDigest)> {
    let mut blobs = (0..count)
        .map(|index| store_blob(state, format!("blob-{index}").as_bytes()))
        .collect::<Vec<_>>();
    blobs.sort_by(|(left, _), (right, _)| left.as_str().cmp(right.as_str()));
    blobs
}

fn tombstoned(meta: &MetaStore) -> Vec<ArtifactDigest> {
    meta.reclamation_tombstones()
        .unwrap()
        .into_iter()
        .map(|record| record.digest)
        .collect()
}

fn pass(state: &Runtime, batch: NonZeroUsize, cancelled: &(dyn Fn() -> bool + Send + Sync)) {
    selector(&[], covered())
        .reclaim_pass(&state.meta, &state.blobs, &state.clock, cancelled, 9, batch)
        .unwrap();
}

#[test]
fn test_successive_passes_cover_the_digests_beyond_the_first_batch() {
    let (_meta_dir, meta) = meta();
    let (_app_dir, state) = app(meta);
    let blobs = ordered_blobs(&state, 5);
    let artifacts = blobs.iter().map(|(_, artifact)| artifact.clone()).collect::<Vec<_>>();
    let two = NonZeroUsize::new(2).unwrap();

    let covered = (0..3)
        .map(|_| {
            pass(&state, two, &|| false);
            tombstoned(&state.meta)
        })
        .collect::<Vec<_>>();

    assert_eq!(
        covered,
        vec![artifacts[..2].to_vec(), artifacts[..4].to_vec(), artifacts.clone()]
    );
}

#[test]
fn test_a_restarted_pass_resumes_from_the_recorded_cursor() {
    let meta_dir = tempfile::tempdir().unwrap();
    let path = meta_dir.path().join("peryx.redb");
    let blob_dir = tempfile::tempdir().unwrap();
    let blobs = BlobStorage::filesystem(blob_dir.path().join("blobs"));
    let clock: Clock = Arc::new(|| 42);
    let started = Runtime {
        meta: crate::support::distributed_meta(&path),
        blobs: blobs.clone(),
        clock: Arc::clone(&clock),
    };
    let artifacts = ordered_blobs(&started, 3)
        .into_iter()
        .map(|(_, artifact)| artifact)
        .collect::<Vec<_>>();
    pass(&started, NonZeroUsize::MIN, &|| false);
    drop(started);

    let restarted = Runtime {
        meta: MetaStore::open_existing(&path).unwrap(),
        blobs,
        clock,
    };
    pass(&restarted, NonZeroUsize::MIN, &|| false);

    assert_eq!(tombstoned(&restarted.meta), artifacts[..2].to_vec());
}

#[test]
fn test_a_completed_scan_wraps_back_to_the_first_digest() {
    let (_meta_dir, meta) = meta();
    let (_app_dir, state) = app(meta);
    let blobs = ordered_blobs(&state, 2);
    pass(&state, NonZeroUsize::MIN, &|| false);
    pass(&state, NonZeroUsize::MIN, &|| false);
    let first = state.meta.reclamation_tombstone(&blobs[0].1).unwrap().unwrap();
    assert!(state.meta.compare_and_remove_reclamation_tombstone(&first).unwrap());

    pass(&state, NonZeroUsize::MIN, &|| false);

    assert_eq!(
        tombstoned(&state.meta),
        vec![blobs[0].1.clone(), blobs[1].1.clone()],
        "the wrapped pass reselects the digest the scan started from"
    );
}

#[test]
fn test_deleting_the_cursor_blob_does_not_block_selection() {
    let (_meta_dir, meta) = meta();
    let (_app_dir, state) = app(meta);
    let blobs = ordered_blobs(&state, 3);
    pass(&state, NonZeroUsize::MIN, &|| false);
    assert!(state.blobs.blocking().delete(&blobs[0].0).unwrap());

    pass(&state, NonZeroUsize::MIN, &|| false);

    assert_eq!(tombstoned(&state.meta), vec![blobs[0].1.clone(), blobs[1].1.clone()]);
}

#[test]
fn test_a_cancelled_pass_leaves_the_cursor_where_it_was() {
    let (_meta_dir, meta) = meta();
    let (_app_dir, state) = app(meta);
    let blobs = ordered_blobs(&state, 2);
    pass(&state, NonZeroUsize::MIN, &|| true);

    pass(&state, NonZeroUsize::MIN, &|| false);

    assert_eq!(
        tombstoned(&state.meta),
        vec![blobs[0].1.clone()],
        "the abandoned page is retried rather than skipped"
    );
}

#[test]
fn test_finalization_advances_one_page_per_pass() {
    let (_meta_dir, meta) = meta();
    let artifacts = ["a", "b", "c"].map(|seed| ArtifactDigest::from_sha256(seed.repeat(64)).unwrap());
    for artifact in &artifacts {
        select_reclamation_candidate(&meta, artifact, false, 0, 0, 9, 10).unwrap();
    }
    let (_app_dir, state) = app(meta);

    let statuses = (0..2)
        .map(|_| {
            pass(&state, NonZeroUsize::MIN, &|| false);
            artifacts
                .iter()
                .map(|artifact| tombstone_status(&state.meta, artifact))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();

    assert_eq!(
        statuses,
        vec![
            vec![
                Some(ReclamationStatus::Ready),
                Some(ReclamationStatus::Pending),
                Some(ReclamationStatus::Pending),
            ],
            vec![
                Some(ReclamationStatus::Ready),
                Some(ReclamationStatus::Ready),
                Some(ReclamationStatus::Pending),
            ],
        ]
    );
}

#[test]
fn test_reclaim_pass_surfaces_a_cursor_read_failure() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    crate::support::distributed_meta(&path);
    let database = Database::open(&path).unwrap();
    let write = database.begin_write().unwrap();
    write
        .open_table(TableDefinition::<&str, u64>::new("reclamation_cursor"))
        .unwrap();
    write.commit().unwrap();
    drop(database);
    let (_app_dir, state) = app(MetaStore::open_existing(&path).unwrap());

    let error = selector(&[], covered())
        .reclaim_pass(&state.meta, &state.blobs, &state.clock, &|| false, 9, batch())
        .unwrap_err();

    assert_eq!(error.code(), "reclamation_cursor_read");
}

#[test]
fn test_reclaim_pass_surfaces_a_cursor_write_failure() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    crate::support::distributed_meta(&path);
    let (_app_dir, state) = app(MetaStore::open_existing_read_only(&path).unwrap());

    let error = selector(&[], covered())
        .reclaim_pass(&state.meta, &state.blobs, &state.clock, &|| false, 9, batch())
        .unwrap_err();

    assert_eq!(error.code(), "reclamation_cursor_write");
}

/// Answers each scan in turn from `answers`, and when `tear_at` names a scan it commits a driver row
/// part way through it, moving the reference revision the way a publish landing between two component
/// scans moves it.
struct ScriptedRefs {
    meta: MetaStore,
    answers: Vec<BTreeSet<String>>,
    tear_at: Option<usize>,
    scans: std::sync::atomic::AtomicUsize,
}

impl ScriptedRefs {
    fn new(meta: &MetaStore, answers: Vec<BTreeSet<String>>, tear_at: Option<usize>) -> Arc<Self> {
        Arc::new(Self {
            meta: meta.clone(),
            answers,
            tear_at,
            scans: std::sync::atomic::AtomicUsize::new(0),
        })
    }
}

impl ReferenceInventory for ScriptedRefs {
    fn referenced(&self) -> Result<BTreeSet<String>, String> {
        let scan = self.scans.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if self.tear_at == Some(scan) {
            publish_reference(&self.meta);
        }
        Ok(self.answers[scan].clone())
    }
}

fn publish_reference(meta: &MetaStore) {
    meta.put_driver_value("published", b"reference").unwrap();
}

fn scripted(state: &Runtime, answers: Vec<BTreeSet<String>>, tear_at: Option<usize>) -> BlobReclamationSelector {
    BlobReclamationSelector::new(
        ScriptedRefs::new(&state.meta, answers, tear_at),
        Arc::new(StubFrontiers(covered())),
    )
}

fn only(digest: &ArtifactDigest) -> BTreeSet<String> {
    BTreeSet::from([digest.sha256().to_owned()])
}

#[test]
fn test_a_reference_committed_after_selection_keeps_the_digest_from_becoming_ready() {
    let (_meta_dir, meta) = meta();
    let (_app_dir, state) = app(meta);
    let (_blob, artifact) = store_blob(&state, b"published while the pass ran");

    let report = scripted(&state, vec![BTreeSet::new(), only(&artifact)], None)
        .reclaim_pass(&state.meta, &state.blobs, &state.clock, &|| false, 9, batch())
        .unwrap();

    assert_eq!(report.processed, 1);
    assert_eq!(
        tombstone_status(&state.meta, &artifact),
        Some(ReclamationStatus::Skipped),
        "the readiness verdict comes from a proof taken after selection, not from the opening scan"
    );
}

#[test]
fn test_an_inventory_torn_by_a_concurrent_commit_selects_nothing() {
    let (_meta_dir, meta) = meta();
    let (_app_dir, state) = app(meta);
    let (_blob, artifact) = store_blob(&state, b"orphan");

    let report = scripted(&state, vec![BTreeSet::new()], Some(0))
        .reclaim_pass(&state.meta, &state.blobs, &state.clock, &|| false, 9, batch())
        .unwrap();

    assert_eq!(report, AvailabilityTaskReport::default());
    assert_eq!(
        tombstone_status(&state.meta, &artifact),
        None,
        "a reference committed between two component scans retires the inventory"
    );
}

#[test]
fn test_an_inventory_torn_after_selection_finalizes_nothing() {
    let (_meta_dir, meta) = meta();
    let (_app_dir, state) = app(meta);
    let (_blob, artifact) = store_blob(&state, b"orphan");

    let report = scripted(&state, vec![BTreeSet::new(), BTreeSet::new()], Some(1))
        .reclaim_pass(&state.meta, &state.blobs, &state.clock, &|| false, 9, batch())
        .unwrap();

    assert_eq!(report.changed, 1, "selection ran against its own proof");
    assert_eq!(
        tombstone_status(&state.meta, &artifact),
        Some(ReclamationStatus::Pending),
        "the readiness proof is retired before any tombstone is marked ready"
    );
}

/// Commits a reference once the pass has already proved its inventory, using the cancellation probe as
/// the point between the proof and the compare-and-put write.
fn publish_between(meta: &MetaStore) -> impl Fn() -> bool + Send + Sync {
    let meta = meta.clone();
    let published = std::sync::atomic::AtomicBool::new(false);
    move || {
        if !published.swap(true, std::sync::atomic::Ordering::Relaxed) {
            publish_reference(&meta);
        }
        false
    }
}

#[test]
fn test_a_reference_committed_after_the_selection_proof_writes_no_tombstone() {
    let (_meta_dir, meta) = meta();
    let (_app_dir, state) = app(meta);
    let (_blob, artifact) = store_blob(&state, b"orphan");

    let report = selector(&[], covered())
        .reclaim_pass(
            &state.meta,
            &state.blobs,
            &state.clock,
            &publish_between(&state.meta),
            9,
            batch(),
        )
        .unwrap();

    assert_eq!(report.processed, 1);
    assert_eq!(report.changed, 0);
    assert_eq!(tombstone_status(&state.meta, &artifact), None);
}

#[test]
fn test_a_reference_committed_after_the_readiness_proof_leaves_the_tombstone_pending() {
    let (_meta_dir, meta) = meta();
    let artifact = ArtifactDigest::from_sha256("7".repeat(64)).unwrap();
    select_reclamation_candidate(&meta, &artifact, false, 0, 0, 9, 10).unwrap();
    let (_app_dir, state) = app(meta);

    let report = selector(&[], covered())
        .reclaim_pass(
            &state.meta,
            &state.blobs,
            &state.clock,
            &publish_between(&state.meta),
            9,
            batch(),
        )
        .unwrap();

    assert_eq!(report, AvailabilityTaskReport::default());
    assert_eq!(
        tombstone_status(&state.meta, &artifact),
        Some(ReclamationStatus::Pending),
        "the write carries the revision its verdict was proved against"
    );
}

#[test]
fn test_reclaim_pass_surfaces_a_reference_revision_read_failure() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    crate::support::distributed_meta(&path);
    let database = Database::open(&path).unwrap();
    let write = database.begin_write().unwrap();
    write
        .delete_table(TableDefinition::<&str, u64>::new("reference_revision"))
        .unwrap();
    write.commit().unwrap();
    drop(database);
    let (_app_dir, state) = app(MetaStore::open_existing(&path).unwrap());

    let error = selector(&[], covered())
        .reclaim_pass(&state.meta, &state.blobs, &state.clock, &|| false, 9, batch())
        .unwrap_err();

    assert_eq!(error.code(), "reclamation_reference_revision");
}
