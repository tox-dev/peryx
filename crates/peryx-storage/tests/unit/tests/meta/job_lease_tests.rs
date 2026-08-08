use tempfile::TempDir;

use crate::meta::{ClaimOutcome, JobLeaseError, LeaseState, MetaStore};

const JOB: &str = "reclaim-sweep";
const TTL: i64 = 30;

fn store() -> (TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, store)
}

#[test]
fn test_first_claim_grants_the_lease_to_the_holder() {
    let (_dir, store) = store();
    let outcome = store.claim_job_lease(JOB, "node-a", 1, 100, TTL).unwrap();
    let ClaimOutcome::Granted(lease) = outcome else {
        panic!("a first claim is granted, got {outcome:?}");
    };
    assert_eq!(lease.holder, "node-a");
    assert_eq!(lease.epoch, 1);
    assert_eq!(lease.state, LeaseState::Held);
    assert_eq!(lease.claimed_at_unix, 100);
    assert_eq!(lease.renewed_at_unix, 100);
    assert_eq!(lease.expires_at_unix, 130);
}

#[test]
fn test_reclaim_by_the_same_holder_and_epoch_renews_and_extends_the_deadline() {
    let (_dir, store) = store();
    store.claim_job_lease(JOB, "node-a", 1, 100, TTL).unwrap();
    let outcome = store.claim_job_lease(JOB, "node-a", 1, 120, TTL).unwrap();
    let ClaimOutcome::Renewed(lease) = outcome else {
        panic!("the holder re-claiming its own epoch renews, got {outcome:?}");
    };
    assert_eq!(lease.claimed_at_unix, 100);
    assert_eq!(lease.renewed_at_unix, 120);
    assert_eq!(lease.expires_at_unix, 150);
}

#[test]
fn test_a_newer_epoch_supersedes_and_reclaims_from_a_live_holder() {
    let (_dir, store) = store();
    store.claim_job_lease(JOB, "node-a", 1, 100, TTL).unwrap();
    let outcome = store.claim_job_lease(JOB, "node-b", 2, 110, TTL).unwrap();
    let ClaimOutcome::Granted(lease) = outcome else {
        panic!("a newer epoch supersedes as a grant, got {outcome:?}");
    };
    assert_eq!(lease.holder, "node-b");
    assert_eq!(lease.epoch, 2);
    assert_eq!(lease.claimed_at_unix, 110);
}

#[test]
fn test_a_different_holder_at_the_same_epoch_takes_the_lease() {
    let (_dir, store) = store();
    store.claim_job_lease(JOB, "node-a", 5, 100, TTL).unwrap();
    let outcome = store.claim_job_lease(JOB, "node-b", 5, 105, TTL).unwrap();
    assert!(matches!(outcome, ClaimOutcome::Granted(_)));
    assert_eq!(store.job_lease(JOB).unwrap().unwrap().holder, "node-b");
}

#[test]
fn test_a_stale_epoch_worker_is_fenced_out() {
    let (_dir, store) = store();
    store.claim_job_lease(JOB, "node-b", 5, 100, TTL).unwrap();
    let error = store.claim_job_lease(JOB, "node-a", 3, 110, TTL).unwrap_err();
    assert!(matches!(error, JobLeaseError::StaleFence { current: 5, applied: 3 }));
    let lease = store.job_lease(JOB).unwrap().unwrap();
    assert_eq!(lease.holder, "node-b");
    assert_eq!(lease.epoch, 5);
}

#[test]
fn test_release_frees_the_lease_for_the_next_claim() {
    let (_dir, store) = store();
    store.claim_job_lease(JOB, "node-a", 1, 100, TTL).unwrap();
    assert!(store.release_job_lease(JOB, "node-a", 1).unwrap());
    assert_eq!(store.job_lease(JOB).unwrap().unwrap().state, LeaseState::Released);
    let outcome = store.claim_job_lease(JOB, "node-b", 1, 110, TTL).unwrap();
    let ClaimOutcome::Granted(lease) = outcome else {
        panic!("a released lease is granted afresh, got {outcome:?}");
    };
    assert_eq!(lease.holder, "node-b");
    assert_eq!(lease.claimed_at_unix, 110);
}

#[test]
fn test_release_of_an_absent_lease_reports_false() {
    let (_dir, store) = store();
    assert!(!store.release_job_lease(JOB, "node-a", 1).unwrap());
}

#[test]
fn test_release_of_an_already_released_lease_reports_false() {
    let (_dir, store) = store();
    store.claim_job_lease(JOB, "node-a", 1, 100, TTL).unwrap();
    store.release_job_lease(JOB, "node-a", 1).unwrap();
    assert!(!store.release_job_lease(JOB, "node-a", 1).unwrap());
}

#[test]
fn test_a_non_holder_cannot_release_a_live_lease() {
    let (_dir, store) = store();
    store.claim_job_lease(JOB, "node-a", 5, 100, TTL).unwrap();
    let error = store.release_job_lease(JOB, "node-b", 5).unwrap_err();
    assert!(matches!(error, JobLeaseError::NotHolder { holder } if holder == "node-a"));
    assert_eq!(store.job_lease(JOB).unwrap().unwrap().state, LeaseState::Held);
}

#[test]
fn test_a_stale_epoch_cannot_release_a_lease_a_newer_epoch_owns() {
    let (_dir, store) = store();
    store.claim_job_lease(JOB, "node-a", 5, 100, TTL).unwrap();
    let error = store.release_job_lease(JOB, "node-a", 3).unwrap_err();
    assert!(matches!(error, JobLeaseError::StaleFence { current: 5, applied: 3 }));
    assert_eq!(store.job_lease(JOB).unwrap().unwrap().state, LeaseState::Held);
}

#[test]
fn test_is_expired_tracks_the_deadline_only_while_held() {
    let (_dir, store) = store();
    let held = store
        .claim_job_lease(JOB, "node-a", 1, 100, TTL)
        .unwrap()
        .lease()
        .clone();
    assert!(!held.is_expired(129));
    assert!(held.is_expired(130));
    store.release_job_lease(JOB, "node-a", 1).unwrap();
    let released = store.job_lease(JOB).unwrap().unwrap();
    assert!(!released.is_expired(1_000));
}

#[test]
fn test_job_leases_lists_every_lease_in_job_order() {
    let (_dir, store) = store();
    store.claim_job_lease("sync", "node-a", 1, 0, TTL).unwrap();
    store.claim_job_lease("build", "node-b", 1, 0, TTL).unwrap();
    let jobs: Vec<_> = store.job_leases().unwrap().into_iter().map(|lease| lease.job).collect();
    assert_eq!(jobs, vec!["build".to_owned(), "sync".to_owned()]);
}

#[test]
fn test_absent_lease_reads_as_none() {
    let (_dir, store) = store();
    assert!(store.job_lease(JOB).unwrap().is_none());
    assert!(store.job_leases().unwrap().is_empty());
}

#[test]
fn test_a_held_lease_survives_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let held;
    {
        let store = MetaStore::open(&path).unwrap();
        held = store
            .claim_job_lease(JOB, "node-a", 7, 100, TTL)
            .unwrap()
            .lease()
            .clone();
    }
    let store = MetaStore::open_existing(&path).unwrap();
    assert_eq!(store.job_lease(JOB).unwrap(), Some(held));
}

#[test]
fn test_a_stale_claim_after_a_release_still_loses_the_fence() {
    let (_dir, store) = store();
    store.claim_job_lease(JOB, "node-b", 5, 100, TTL).unwrap();
    store.release_job_lease(JOB, "node-b", 5).unwrap();
    let error = store.claim_job_lease(JOB, "node-a", 4, 110, TTL).unwrap_err();
    assert!(matches!(error, JobLeaseError::StaleFence { current: 5, applied: 4 }));
}
