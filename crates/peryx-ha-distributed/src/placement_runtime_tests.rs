use std::path::PathBuf;
use std::sync::Arc;

use peryx_driver::state::{AppState, ClusterStatus, HomeClaim, OwnershipAuthority, OwnershipError, TransferOutcome};
use peryx_identity::{ArtifactDigest, RevocationReason, UserId};
use peryx_storage::blob::{BlobStorage, BlobStore, Digest};
use peryx_storage::meta::{
    BackendId, BackendLocation, BlobPlacementKey, BlobPlacementState, BlobPlacementStatus, BlobPlacementTransition,
    MetaStore,
};

use super::*;

const CONTENT: &[u8] = b"placement reconcile artifact bytes";

#[test]
fn test_task_error_preserves_the_job_failure() {
    let error = task_error(JobFailure::new("reconcile_failed", "store unavailable"));

    assert_eq!(error.code(), "reconcile_failed");
    assert_eq!(error.message(), "store unavailable");
}

fn digests(content: &[u8]) -> (Digest, ArtifactDigest) {
    let blob = Digest::of(content);
    let artifact = ArtifactDigest::from_sha256(blob.as_str()).unwrap();
    (blob, artifact)
}

fn meta() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, store)
}

/// A filesystem blob store plus the content-addressed root, so a test can plant a rotted file directly.
fn filesystem() -> (tempfile::TempDir, BlobStore, PathBuf, BackendId) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("blobs");
    let blobs = BlobStorage::filesystem(root.clone());
    let store = blobs.filesystem_store().unwrap().clone();
    let backend = blobs.backend_id();
    (dir, store, root, backend)
}

fn dc(name: &str) -> DataCenterId {
    DataCenterId::new(name).unwrap()
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

fn blob_path(root: &std::path::Path, digest: &Digest) -> PathBuf {
    let hex = digest.as_str();
    root.join("sha256").join(&hex[0..2]).join(&hex[2..4]).join(hex)
}

fn reconciler(local: &str, store: BlobStore, targets: &[&str]) -> FilesystemPlacementReconciler {
    FilesystemPlacementReconciler {
        local_dc: dc(local),
        store,
        target_dcs: targets.iter().map(|name| dc(name)).collect(),
    }
}

fn state_state(meta: MetaStore) -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("state-blobs"));
    (dir, Arc::new(AppState::new(meta, blobs, 60, Vec::new())))
}

fn clock() -> Clock {
    Arc::new(|| 42)
}

#[test]
fn test_is_withdrawn_is_false_for_a_live_digest() {
    let (_dir, meta) = meta();
    let (_blob, artifact) = digests(CONTENT);

    assert!(!is_withdrawn(&meta, &artifact).unwrap());
}

#[test]
fn test_is_withdrawn_is_true_under_an_active_revocation() {
    let (_dir, meta) = meta();
    let (_blob, artifact) = digests(CONTENT);
    meta.put_digest_revocation(&artifact, &RevocationReason::new("bad").unwrap(), &UserId::random(), 5)
        .unwrap();

    assert!(is_withdrawn(&meta, &artifact).unwrap());
}

#[test]
fn test_is_withdrawn_is_false_after_a_revocation_is_lifted() {
    let (_dir, meta) = meta();
    let (_blob, artifact) = digests(CONTENT);
    let actor = UserId::random();
    meta.put_digest_revocation(&artifact, &RevocationReason::new("bad").unwrap(), &actor, 5)
        .unwrap();
    meta.lift_digest_revocation(&artifact, &actor, 6).unwrap();

    assert!(!is_withdrawn(&meta, &artifact).unwrap());
}

#[test]
fn test_is_withdrawn_is_true_under_an_in_flight_reclamation() {
    let (_dir, meta) = meta();
    let (_blob, artifact) = digests(CONTENT);
    // An unreferenced, unservable digest is selected into a pending reclamation tombstone.
    meta.select_reclamation_candidate(&artifact, false, 0, 5, 10).unwrap();

    assert!(is_withdrawn(&meta, &artifact).unwrap());
}

#[test]
fn test_is_withdrawn_is_false_for_a_skipped_reclamation() {
    let (_dir, meta) = meta();
    let (_blob, artifact) = digests(CONTENT);
    // A still-referenced candidate is skipped rather than deleted, so it is not being withdrawn.
    meta.select_reclamation_candidate(&artifact, true, 0, 5, 10).unwrap();

    assert!(!is_withdrawn(&meta, &artifact).unwrap());
}

fn placement_state(meta: &MetaStore, key: &BlobPlacementKey) -> BlobPlacementState {
    meta.blob_placement(key).unwrap().unwrap().state
}

#[test]
fn test_repair_leaves_an_intact_copy_alone() {
    let (_dir, meta) = meta();
    let (_sdir, store, _root, backend) = filesystem();
    let (blob, artifact) = digests(CONTENT);
    store.write_verified(CONTENT, &blob).unwrap();
    let placement = key(&artifact, &backend, "home", artifact.sha256());
    seed_verified(&meta, &placement, CONTENT.len() as u64);

    let changed =
        reconciler("home", store.clone(), &["home", "east"]).repair_if_corrupt(&meta, &clock(), 5, &placement);

    assert!(!changed);
    assert_eq!(
        placement_state(&meta, &placement).status(),
        BlobPlacementStatus::Verified
    );
    assert_eq!(store.read(&blob).unwrap(), CONTENT);
}

#[test]
fn test_repair_demotes_and_drops_a_rotted_copy() {
    let (_dir, meta) = meta();
    let (_sdir, store, root, backend) = filesystem();
    let (blob, artifact) = digests(CONTENT);
    store.write_verified(CONTENT, &blob).unwrap();
    std::fs::write(blob_path(&root, &blob), b"rotted bytes").unwrap();
    let placement = key(&artifact, &backend, "home", artifact.sha256());
    seed_verified(&meta, &placement, CONTENT.len() as u64);

    let changed =
        reconciler("home", store.clone(), &["home", "east"]).repair_if_corrupt(&meta, &clock(), 5, &placement);

    assert!(changed);
    assert_eq!(
        placement_state(&meta, &placement),
        BlobPlacementState::Failed {
            class: BlobPlacementFailure::DigestMismatch
        }
    );
    assert!(
        store.read(&blob).is_err(),
        "the corrupt bytes are dropped so the backlog re-copies"
    );
}

#[cfg(unix)]
#[test]
fn test_repair_warns_when_the_corrupt_bytes_cannot_be_dropped() {
    use std::os::unix::fs::PermissionsExt as _;

    let (_dir, meta) = meta();
    let (_sdir, store, root, backend) = filesystem();
    let (blob, artifact) = digests(CONTENT);
    store.write_verified(CONTENT, &blob).unwrap();
    // Rot the stored bytes so the copy fails verification, then make the blob's parent directory
    // read-only so unlinking the demoted file is refused: the demotion still stands, the pass still
    // counts the change, and the bad bytes stay behind under a logged warning.
    let path = blob_path(&root, &blob);
    std::fs::write(&path, b"rotted bytes").unwrap();
    let parent = path.parent().unwrap().to_path_buf();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o555)).unwrap();
    let placement = key(&artifact, &backend, "home", artifact.sha256());
    seed_verified(&meta, &placement, CONTENT.len() as u64);

    let changed =
        reconciler("home", store.clone(), &["home", "east"]).repair_if_corrupt(&meta, &clock(), 5, &placement);

    // Restore write permission so the temporary directory can be torn down.
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert!(
        changed,
        "a copy whose bad bytes could not be dropped is still demoted and counted"
    );
    assert_eq!(
        placement_state(&meta, &placement),
        BlobPlacementState::Failed {
            class: BlobPlacementFailure::DigestMismatch
        }
    );
    assert_eq!(
        store.read(&blob).unwrap(),
        b"rotted bytes",
        "the bytes remain because their removal was refused"
    );
}

#[test]
fn test_repair_demotes_a_verified_record_over_a_missing_file() {
    let (_dir, meta) = meta();
    let (_sdir, store, _root, backend) = filesystem();
    let (_blob, artifact) = digests(CONTENT);
    let placement = key(&artifact, &backend, "home", artifact.sha256());
    seed_verified(&meta, &placement, CONTENT.len() as u64); // record says verified, but no file was written

    let changed = reconciler("home", store, &["home", "east"]).repair_if_corrupt(&meta, &clock(), 5, &placement);

    assert!(changed);
    assert_eq!(
        placement_state(&meta, &placement),
        BlobPlacementState::Failed {
            class: BlobPlacementFailure::DigestMismatch
        }
    );
}

#[test]
fn test_repair_leaves_a_copy_it_cannot_read_alone() {
    let (_dir, meta) = meta();
    let (_sdir, store, root, backend) = filesystem();
    let (blob, artifact) = digests(CONTENT);
    // A directory where the blob file should be makes a re-verify read fail with a non-not-found error.
    let path = blob_path(&root, &blob);
    std::fs::create_dir_all(&path).unwrap();
    let placement = key(&artifact, &backend, "home", artifact.sha256());
    seed_verified(&meta, &placement, CONTENT.len() as u64);

    let changed = reconciler("home", store, &["home", "east"]).repair_if_corrupt(&meta, &clock(), 5, &placement);

    assert!(
        !changed,
        "an unreadable copy is left for the next pass rather than demoted"
    );
    assert_eq!(
        placement_state(&meta, &placement).status(),
        BlobPlacementStatus::Verified
    );
}

#[test]
fn test_repair_that_the_fence_rejects_makes_no_change() {
    let (_dir, meta) = meta();
    let (_sdir, store, _root, backend) = filesystem();
    let (_blob, artifact) = digests(CONTENT);
    let placement = key(&artifact, &backend, "home", artifact.sha256());
    seed_verified(&meta, &placement, CONTENT.len() as u64); // fenced at epoch 1, no local file

    // A lower fence than the record's is a stale writer, so the demotion is rejected and nothing changes.
    let changed = reconciler("home", store, &["home", "east"]).repair_if_corrupt(&meta, &clock(), 0, &placement);

    assert!(!changed);
    assert_eq!(
        placement_state(&meta, &placement).status(),
        BlobPlacementStatus::Verified
    );
}

#[test]
fn test_verify_pass_repairs_corrupt_and_skips_intact_and_withdrawn() {
    let (_dir, meta) = meta();
    let (_sdir, store, _root, backend) = filesystem();
    let (_b1, corrupt) = digests(b"corrupt-one");
    let (_b2, intact) = digests(b"intact-two");
    let (_b3, withdrawn) = digests(b"withdrawn-three");
    store.write_verified(b"intact-two", &Digest::of(b"intact-two")).unwrap();
    let corrupt_key = key(&corrupt, &backend, "home", corrupt.sha256());
    let intact_key = key(&intact, &backend, "home", intact.sha256());
    let withdrawn_key = key(&withdrawn, &backend, "home", withdrawn.sha256());
    seed_verified(&meta, &corrupt_key, 1); // no file -> corrupt
    seed_verified(&meta, &intact_key, 10);
    seed_verified(&meta, &withdrawn_key, 1);
    meta.put_digest_revocation(&withdrawn, &RevocationReason::new("bad").unwrap(), &UserId::random(), 5)
        .unwrap();

    let tally = reconciler("home", store, &["home", "east"])
        .verify_local_placements(&meta, &clock(), 5, 100, &|| false)
        .unwrap();

    assert_eq!(tally.scanned, 3);
    assert_eq!(tally.changed, 1, "only the corrupt, non-withdrawn copy is demoted");
    assert_eq!(
        placement_state(&meta, &intact_key).status(),
        BlobPlacementStatus::Verified
    );
    assert_eq!(
        placement_state(&meta, &withdrawn_key).status(),
        BlobPlacementStatus::Verified
    );
}

#[test]
fn test_verify_pass_pages_across_the_cursor() {
    let (_dir, meta) = meta();
    let (_sdir, store, _root, backend) = filesystem();
    for tag in ["aa", "bb"] {
        let (_blob, artifact) = digests(tag.as_bytes());
        seed_verified(&meta, &key(&artifact, &backend, "home", artifact.sha256()), 1); // no file -> corrupt
    }

    // A batch of one forces a resume past the cursor for the second placement.
    let tally = reconciler("home", store, &["home", "east"])
        .verify_local_placements(&meta, &clock(), 5, 1, &|| false)
        .unwrap();

    assert_eq!(tally.changed, 2);
}

#[test]
fn test_verify_pass_stops_when_cancelled() {
    let (_dir, meta) = meta();
    let (_sdir, store, _root, backend) = filesystem();
    let (_blob, artifact) = digests(CONTENT);
    seed_verified(&meta, &key(&artifact, &backend, "home", artifact.sha256()), 1);

    let tally = reconciler("home", store, &["home", "east"])
        .verify_local_placements(&meta, &clock(), 5, 100, &|| true)
        .unwrap();

    assert_eq!(tally, PassTally::default(), "a cancelled pass touches nothing");
}

#[test]
fn test_verify_pass_breaks_within_a_page_when_cancelled() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let (_dir, meta) = meta();
    let (_sdir, store, _root, backend) = filesystem();
    for tag in ["aa", "bb"] {
        let (_blob, artifact) = digests(tag.as_bytes());
        seed_verified(&meta, &key(&artifact, &backend, "home", artifact.sha256()), 1); // no file -> corrupt
    }
    // The cancellation clears the outer check and the first record, then trips on the second, so the
    // pass breaks partway through a single page rather than at the page boundary.
    let calls = AtomicUsize::new(0);
    let cancelled = || calls.fetch_add(1, Ordering::SeqCst) >= 2;

    let tally = reconciler("home", store, &["home", "east"])
        .verify_local_placements(&meta, &clock(), 5, 100, &cancelled)
        .unwrap();

    assert_eq!(tally.scanned, 1, "the second record in the page was left unscanned");
    assert_eq!(tally.changed, 1);
}

#[test]
fn test_verify_pass_surfaces_a_scan_rejection() {
    let (_dir, meta) = meta();
    let (_sdir, store, _root, _backend) = filesystem();

    let error = reconciler("home", store, &["home", "east"])
        .verify_local_placements(&meta, &clock(), 5, 0, &|| false)
        .unwrap_err();

    assert_eq!(error.code(), "placement_verify_scan");
}

#[test]
fn test_verify_pass_surfaces_a_withdrawal_read_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let (_sdir, store, _root, backend) = filesystem();
    let (_blob, artifact) = digests(CONTENT);
    let placement = key(&artifact, &backend, "home", artifact.sha256());
    {
        let meta = MetaStore::open(&path).unwrap();
        seed_verified(&meta, &placement, 1);
    }
    // Plant a corrupt revocation record for the scanned digest so the withdrawal check fails to read it
    // while the placement scan still returns the record, forcing the pass to surface the read error.
    {
        let db = redb::Database::open(&path).unwrap();
        let txn = db.begin_write().unwrap();
        {
            let mut table = txn
                .open_table(redb::TableDefinition::<&str, &[u8]>::new("digest_revocation"))
                .unwrap();
            table
                .insert(artifact.canonical().as_str(), b"not json".as_slice())
                .unwrap();
        }
        txn.commit().unwrap();
    }
    let meta = MetaStore::open(&path).unwrap();

    let error = reconciler("home", store, &["home", "east"])
        .verify_local_placements(&meta, &clock(), 5, 100, &|| false)
        .unwrap_err();

    assert_eq!(error.code(), "placement_withdrawal_read");
}

#[test]
fn test_retire_revokes_an_out_of_policy_copy_and_leaves_policy_copies() {
    let (_dir, meta) = meta();
    let (_sdir, store, _root, backend) = filesystem();
    let (_blob, artifact) = digests(CONTENT);
    let in_policy = key(&artifact, &backend, "east", "east/loc");
    let out_of_policy = key(&artifact, &backend, "old-dc", "old/loc");
    seed_verified(&meta, &in_policy, 1);
    seed_verified(&meta, &out_of_policy, 1);

    let tally = reconciler("home", store, &["home", "east"])
        .retire_out_of_policy(&meta, &clock(), 5, 100, &|| false)
        .unwrap();

    assert_eq!(tally.changed, 1);
    assert_eq!(
        placement_state(&meta, &out_of_policy).status(),
        BlobPlacementStatus::Revoked
    );
    assert_eq!(
        placement_state(&meta, &in_policy).status(),
        BlobPlacementStatus::Verified
    );
}

#[test]
fn test_retire_pages_across_the_cursor() {
    let (_dir, meta) = meta();
    let (_sdir, store, _root, backend) = filesystem();
    for tag in ["aa", "bb"] {
        let (_blob, artifact) = digests(tag.as_bytes());
        seed_verified(&meta, &key(&artifact, &backend, "old-dc", &format!("old/{tag}")), 1);
    }

    // A batch of one forces the retire pass to resume past the cursor for the second digest.
    let tally = reconciler("home", store, &["home", "east"])
        .retire_out_of_policy(&meta, &clock(), 5, 1, &|| false)
        .unwrap();

    assert_eq!(tally.changed, 2, "both out-of-policy copies retire across two pages");
}

#[test]
fn test_retire_stops_when_cancelled() {
    let (_dir, meta) = meta();
    let (_sdir, store, _root, backend) = filesystem();
    let (_blob, artifact) = digests(CONTENT);
    seed_verified(&meta, &key(&artifact, &backend, "old-dc", "old/loc"), 1);

    let tally = reconciler("home", store, &["home", "east"])
        .retire_out_of_policy(&meta, &clock(), 5, 100, &|| true)
        .unwrap();

    assert_eq!(tally, PassTally::default());
}

#[test]
fn test_retire_surfaces_a_scan_rejection() {
    let (_dir, meta) = meta();
    let (_sdir, store, _root, _backend) = filesystem();

    let error = reconciler("home", store, &["home", "east"])
        .retire_out_of_policy(&meta, &clock(), 5, 0, &|| false)
        .unwrap_err();

    assert_eq!(error.code(), "placement_reconcile_scan");
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
async fn test_reconcile_pass_is_fenced_shut_without_a_cluster_term() {
    let (_dir, meta) = meta();
    let (_sdir, store, _root, backend) = filesystem();
    let (_blob, artifact) = digests(CONTENT);
    let (_adir, state) = state_state(meta);
    seed_verified(&state.meta, &key(&artifact, &backend, "home", artifact.sha256()), 1);

    let report = reconciler("home", store, &["home", "east"])
        .bind(state.serving.clone())
        .reconcile_pass(&|| false, std::num::NonZeroUsize::new(100).unwrap())
        .await
        .unwrap();

    assert_eq!(
        report,
        peryx_ha::AvailabilityTaskReport::default(),
        "no cluster term fences reconciliation shut"
    );
}

#[tokio::test]
async fn test_reconcile_pass_repairs_and_retires_under_a_live_term() {
    let (_dir, meta) = meta();
    let (_sdir, store, _root, backend) = filesystem();
    let (_b1, corrupt) = digests(b"corrupt-local");
    let (_b2, surplus) = digests(b"surplus-remote");
    let (_adir, state) = state_state(meta);
    seed_verified(&state.meta, &key(&corrupt, &backend, "home", corrupt.sha256()), 1); // local, no file -> corrupt
    seed_verified(&state.meta, &key(&surplus, &backend, "old-dc", "old/loc"), 1); // out-of-policy
    state.set_ownership_authority(Arc::new(FixedTerm(9)));

    let report = reconciler("home", store, &["home", "east"])
        .reconcile_pass(&state, &|| false, PlacementReconcileParameters::new())
        .unwrap();

    assert_eq!(report.changed, 2, "one demotion and one retirement");
    assert_eq!(
        placement_state(&state.meta, &key(&corrupt, &backend, "home", corrupt.sha256())).status(),
        BlobPlacementStatus::Failed
    );
    assert_eq!(
        placement_state(&state.meta, &key(&surplus, &backend, "old-dc", "old/loc")).status(),
        BlobPlacementStatus::Revoked
    );
}

#[test]
fn test_reconciler_requires_multiple_datacenters() {
    let (_dir, store, _root, _backend) = filesystem();

    assert!(FilesystemPlacementReconciler::new(dc("home"), store, BTreeSet::from([dc("home")])).is_none());
}
