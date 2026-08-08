use peryx_identity::ArtifactDigest;
use tempfile::TempDir;
use uuid::Uuid;

use crate::blob::{BlobStore, Digest};
use crate::meta::{AccountingClass, MetaStore, NewQuotaReservation, ObservedFrontier, QuotaLimits};
use crate::reclaim::{ReclaimError, ReclaimOutcome, ReclaimRequest, reclaim_ready_blob};

const JOB: &str = "reclaim-sweep";
const HOLDER: &str = "node-a";
const EPOCH: u64 = 5;
const FRONTIER: u64 = 5;

fn setup() -> (TempDir, MetaStore, BlobStore) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    (dir, meta, blobs)
}

fn stored_blob(blobs: &BlobStore, bytes: &[u8]) -> (Digest, ArtifactDigest) {
    let blob = blobs.write(bytes).unwrap();
    let artifact = ArtifactDigest::from_sha256(blob.as_str()).unwrap();
    (blob, artifact)
}

fn hold_lease(meta: &MetaStore, holder: &str, epoch: u64) {
    meta.claim_job_lease(JOB, holder, epoch, 0, 30).unwrap();
}

fn arm_ready(meta: &MetaStore, artifact: &ArtifactDigest, epoch: u64) {
    meta.select_reclamation_candidate(artifact, false, FRONTIER, epoch, 0)
        .unwrap();
    let observed = ObservedFrontier {
        replica: Some(FRONTIER),
        backup: Some(FRONTIER),
    };
    meta.mark_reclamation_ready(artifact, false, observed, epoch, 1)
        .unwrap();
}

fn commit_reservation(meta: &MetaStore, artifact: &ArtifactDigest, bytes: u64) -> Uuid {
    let reservation = meta
        .reserve_quota(
            NewQuotaReservation {
                repository: "private",
                project: None,
                version: None,
                digest: &artifact.canonical(),
                bytes,
                class: AccountingClass::Generated,
                created_at_unix: 0,
            },
            QuotaLimits::default(),
        )
        .unwrap();
    meta.commit_quota_reservation(reservation.id).unwrap();
    reservation.id
}

fn request<'a>(artifact: &'a ArtifactDigest, epoch: u64, reservations: &'a [Uuid]) -> ReclaimRequest<'a> {
    ReclaimRequest {
        digest: artifact,
        job: JOB,
        holder: HOLDER,
        epoch,
        reservations,
    }
}

#[tokio::test]
async fn test_reclaims_a_ready_blob_and_credits_quota() {
    let (_dir, meta, blobs) = setup();
    let (blob, artifact) = stored_blob(&blobs, b"payload");
    hold_lease(&meta, HOLDER, EPOCH);
    arm_ready(&meta, &artifact, EPOCH);
    let reservation = commit_reservation(&meta, &artifact, 7);
    let outcome = reclaim_ready_blob(&meta, &blobs, request(&artifact, EPOCH, &[reservation]))
        .await
        .unwrap();
    assert_eq!(
        outcome,
        ReclaimOutcome::Reclaimed {
            deleted: true,
            credited: 1
        }
    );
    assert!(!blobs.exists(&blob));
    assert!(meta.reclamation_tombstone(&artifact).unwrap().is_none());
    assert!(meta.quota_reservation(reservation).unwrap().is_none());
}

#[tokio::test]
async fn test_reclaims_a_ready_digest_whose_blob_is_already_absent() {
    let (_dir, meta, blobs) = setup();
    let artifact = ArtifactDigest::from_sha256(Digest::of(b"never-written").as_str()).unwrap();
    hold_lease(&meta, HOLDER, EPOCH);
    arm_ready(&meta, &artifact, EPOCH);
    let outcome = reclaim_ready_blob(&meta, &blobs, request(&artifact, EPOCH, &[]))
        .await
        .unwrap();
    assert_eq!(
        outcome,
        ReclaimOutcome::Reclaimed {
            deleted: false,
            credited: 0
        }
    );
    assert!(meta.reclamation_tombstone(&artifact).unwrap().is_none());
}

#[tokio::test]
async fn test_reclaim_prunes_the_blob_fan_out_directories() {
    let (_dir, meta, blobs) = setup();
    let (blob, artifact) = stored_blob(&blobs, b"payload");
    let cd = blobs.path_for(&blob).parent().unwrap().to_path_buf();
    hold_lease(&meta, HOLDER, EPOCH);
    arm_ready(&meta, &artifact, EPOCH);
    reclaim_ready_blob(&meta, &blobs, request(&artifact, EPOCH, &[]))
        .await
        .unwrap();
    assert!(!cd.exists());
}

#[tokio::test]
async fn test_reclaim_credits_a_reservation_at_most_once() {
    let (_dir, meta, blobs) = setup();
    let (_blob, artifact) = stored_blob(&blobs, b"payload");
    hold_lease(&meta, HOLDER, EPOCH);
    arm_ready(&meta, &artifact, EPOCH);
    let reservation = commit_reservation(&meta, &artifact, 7);
    meta.release_quota_reservation(reservation).unwrap();
    let outcome = reclaim_ready_blob(&meta, &blobs, request(&artifact, EPOCH, &[reservation]))
        .await
        .unwrap();
    assert_eq!(
        outcome,
        ReclaimOutcome::Reclaimed {
            deleted: true,
            credited: 0
        }
    );
}

#[tokio::test]
async fn test_reclaim_rejects_a_caller_that_does_not_hold_the_lease() {
    let (_dir, meta, blobs) = setup();
    let (blob, artifact) = stored_blob(&blobs, b"payload");
    hold_lease(&meta, "node-b", EPOCH);
    arm_ready(&meta, &artifact, EPOCH);
    let error = reclaim_ready_blob(&meta, &blobs, request(&artifact, EPOCH, &[]))
        .await
        .unwrap_err();
    assert!(matches!(error, ReclaimError::NotLeaseHolder { epoch: EPOCH, .. }));
    assert!(blobs.exists(&blob));
}

#[tokio::test]
async fn test_reclaim_rejects_when_no_lease_exists() {
    let (_dir, meta, blobs) = setup();
    let (_blob, artifact) = stored_blob(&blobs, b"payload");
    arm_ready(&meta, &artifact, EPOCH);
    let error = reclaim_ready_blob(&meta, &blobs, request(&artifact, EPOCH, &[]))
        .await
        .unwrap_err();
    assert!(matches!(error, ReclaimError::NotLeaseHolder { .. }));
}

#[tokio::test]
async fn test_reclaim_rejects_a_superseded_candidate() {
    let (_dir, meta, blobs) = setup();
    let (blob, artifact) = stored_blob(&blobs, b"payload");
    hold_lease(&meta, HOLDER, 5);
    arm_ready(&meta, &artifact, 7);
    let error = reclaim_ready_blob(&meta, &blobs, request(&artifact, 5, &[]))
        .await
        .unwrap_err();
    assert!(matches!(error, ReclaimError::Superseded { current: 7, applied: 5 }));
    assert!(blobs.exists(&blob));
}

#[tokio::test]
async fn test_reclaim_without_a_candidate_is_missing() {
    let (_dir, meta, blobs) = setup();
    let (_blob, artifact) = stored_blob(&blobs, b"payload");
    hold_lease(&meta, HOLDER, EPOCH);
    let error = reclaim_ready_blob(&meta, &blobs, request(&artifact, EPOCH, &[]))
        .await
        .unwrap_err();
    assert!(matches!(error, ReclaimError::MissingCandidate));
}

#[tokio::test]
async fn test_reclaim_leaves_a_pending_candidate_untouched() {
    let (_dir, meta, blobs) = setup();
    let (blob, artifact) = stored_blob(&blobs, b"payload");
    hold_lease(&meta, HOLDER, EPOCH);
    meta.select_reclamation_candidate(&artifact, false, FRONTIER, EPOCH, 0)
        .unwrap();
    let outcome = reclaim_ready_blob(&meta, &blobs, request(&artifact, EPOCH, &[]))
        .await
        .unwrap();
    assert_eq!(outcome, ReclaimOutcome::NotReady);
    assert!(blobs.exists(&blob));
    assert!(meta.reclamation_tombstone(&artifact).unwrap().is_some());
}

#[tokio::test]
async fn test_reclaim_prunes_an_abandoned_candidate_without_deleting_bytes() {
    let (_dir, meta, blobs) = setup();
    let (blob, artifact) = stored_blob(&blobs, b"payload");
    hold_lease(&meta, HOLDER, EPOCH);
    meta.select_reclamation_candidate(&artifact, false, FRONTIER, EPOCH, 0)
        .unwrap();
    let observed = ObservedFrontier {
        replica: Some(FRONTIER),
        backup: Some(FRONTIER),
    };
    meta.mark_reclamation_ready(&artifact, true, observed, EPOCH, 1)
        .unwrap();
    let outcome = reclaim_ready_blob(&meta, &blobs, request(&artifact, EPOCH, &[]))
        .await
        .unwrap();
    assert_eq!(outcome, ReclaimOutcome::Abandoned);
    assert!(blobs.exists(&blob));
    assert!(meta.reclamation_tombstone(&artifact).unwrap().is_none());
}
