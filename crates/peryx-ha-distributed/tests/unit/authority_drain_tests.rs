use std::num::NonZeroUsize;
use std::sync::Arc;

use async_trait::async_trait;
use peryx_driver::jobs::{JobLimits, JobReport, JobScheduler};
use peryx_driver::state::{
    AppState, ClusterStatus, HomeClaim, OwnershipAuthority, OwnershipError, ServingState, TransferOutcome,
};
use peryx_storage::blob::BlobStorage;
use peryx_storage::meta::{IntentPhase, JobKind, MetaStore};

use super::{AuthorityDrainJob, drain_pending};

fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, store)
}

fn stage(store: &MetaStore, key: &str) {
    let limits = peryx_storage::meta::IntentLimits {
        max_records: 100,
        max_bytes: 1 << 20,
        backpressure_percent: 80,
    };
    let admission = peryx_storage::meta::IntentAdmission {
        authority: "auth",
        key,
        digest: "digest",
        size: 1,
        payload: b"payload",
    };
    store.stage_intent(admission, limits, 1).unwrap();
}

fn batch(size: usize) -> NonZeroUsize {
    NonZeroUsize::new(size).unwrap()
}

#[test]
fn test_drain_finalizes_every_pending_intent_across_batches() {
    let (_dir, store) = store();
    for serial in 0..6 {
        stage(&store, &format!("key-{serial}"));
    }

    let report = drain_pending(&store, batch(4), 9, || false).unwrap();

    assert_eq!(
        report,
        JobReport {
            processed: 6,
            changed: 6
        }
    );
    assert!(store.list_pending_intents(10).unwrap().is_empty());
    for serial in 0..6 {
        let record = store.staged_intent(&format!("key-{serial}")).unwrap().unwrap();
        assert_eq!(record.phase, IntentPhase::Admitted);
        assert_eq!(record.updated_at_unix, 9);
    }
}

#[test]
fn test_drain_resumes_past_already_finalized_intents_and_is_idempotent() {
    let (_dir, store) = store();
    for serial in 0..5 {
        stage(&store, &format!("key-{serial}"));
    }
    // A prior interrupted run finalized two of them.
    store.advance_intent("key-1", IntentPhase::Admitted, 2).unwrap();
    store.advance_intent("key-3", IntentPhase::Admitted, 2).unwrap();

    let report = drain_pending(&store, batch(4), 9, || false).unwrap();

    // Only the three still pending finalize; the settled ones are neither re-finalized nor recounted.
    assert_eq!(
        report,
        JobReport {
            processed: 3,
            changed: 3
        }
    );
    // A re-run over the drained ledger is a no-op, so the drain is safe to retry.
    assert_eq!(
        drain_pending(&store, batch(4), 9, || false).unwrap(),
        JobReport::default()
    );
    for serial in 0..5 {
        assert_eq!(
            store.staged_intent(&format!("key-{serial}")).unwrap().unwrap().phase,
            IntentPhase::Admitted
        );
    }
}

#[test]
fn test_drain_stops_between_batches_when_cancelled() {
    let (_dir, store) = store();
    for serial in 0..8 {
        stage(&store, &format!("key-{serial}"));
    }
    let calls = std::cell::Cell::new(0);

    // Not cancelled on the first check, cancelled before the second batch, so one batch of four drains.
    let report = drain_pending(&store, batch(4), 9, || {
        let seen = calls.get();
        calls.set(seen + 1);
        seen >= 1
    })
    .unwrap();

    assert_eq!(
        report,
        JobReport {
            processed: 4,
            changed: 4
        }
    );
    assert_eq!(store.list_pending_intents(10).unwrap().len(), 4);
}

/// An ownership group double with a fixed committed epoch and a fixed fence verdict, to drive the
/// scheduler's authority fence deterministically.
struct FixedAuthority {
    epoch: u64,
    admit: bool,
}

#[async_trait]
impl OwnershipAuthority for FixedAuthority {
    async fn has_home(&self, _authority: &str) -> bool {
        true
    }

    async fn claim_home(&self, _authority: &str) -> Result<HomeClaim, OwnershipError> {
        Ok(HomeClaim::AlreadyHomed)
    }

    fn cluster_status(&self) -> ClusterStatus {
        ClusterStatus {
            leader: None,
            term: 0,
            voters: Vec::new(),
        }
    }

    async fn committed_epoch(&self, _authority: &str) -> u64 {
        self.epoch
    }

    async fn admit_epoch(&self, _authority: &str, _presented: u64) -> bool {
        self.admit
    }

    async fn transfer_home(
        &self,
        _authority: &str,
        _new_home: &str,
    ) -> Result<Option<TransferOutcome>, OwnershipError> {
        Ok(None)
    }
}

fn state_with_authority(authority: FixedAuthority) -> (tempfile::TempDir, Arc<ServingState>) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let app = AppState::with_clock(meta, blobs, 60, Vec::new(), Arc::new(|| 1_000));
    app.serving.set_ownership_authority(Arc::new(authority));
    (dir, app.serving)
}

#[tokio::test]
async fn test_a_scheduled_drain_finalizes_and_records_an_authority_drain_run() {
    let (_dir, state) = state_with_authority(FixedAuthority { epoch: 5, admit: true });
    stage(&state.meta, "key-1");
    let scheduler = JobScheduler::new(state.clone(), JobLimits::node_local());

    let report = scheduler.run(Arc::new(AuthorityDrainJob::new("proj"))).await.unwrap();

    assert_eq!(
        report,
        JobReport {
            processed: 1,
            changed: 1
        }
    );
    assert_eq!(
        state.meta.staged_intent("key-1").unwrap().unwrap().phase,
        IntentPhase::Admitted
    );
    let runs = state.meta.list_job_runs().unwrap();
    assert_eq!(runs[0].kind, JobKind::AuthorityDrain);
    assert_eq!(runs[0].repository.as_deref(), Some("proj"));
    scheduler.shutdown().await;
}

#[tokio::test]
async fn test_a_drain_whose_authority_was_superseded_mid_run_is_fenced() {
    let (_dir, state) = state_with_authority(FixedAuthority { epoch: 5, admit: false });
    stage(&state.meta, "key-1");
    let scheduler = JobScheduler::new(state.clone(), JobLimits::node_local());

    let error = scheduler
        .run(Arc::new(AuthorityDrainJob::new("proj")))
        .await
        .unwrap_err();

    assert_eq!(error, "authority_fenced: a newer authority epoch superseded this run");
    scheduler.shutdown().await;
}

#[tokio::test]
async fn test_a_drain_surfaces_a_storage_error_as_a_run_failure() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let meta = MetaStore::open(&path).unwrap();
    stage(&meta, "key-1");
    drop(meta);
    let database = redb::Database::open(&path).unwrap();
    let write = database.begin_write().unwrap();
    {
        let mut table = write
            .open_table(redb::TableDefinition::<&str, &[u8]>::new("ingress_intent"))
            .unwrap();
        table.insert("key-1", b"not json".as_slice()).unwrap();
    }
    write.commit().unwrap();
    drop(database);
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let app = AppState::with_clock(
        MetaStore::open_existing(&path).unwrap(),
        blobs,
        60,
        Vec::new(),
        Arc::new(|| 1_000),
    );
    let scheduler = JobScheduler::new(app.serving.clone(), JobLimits::node_local());

    let error = scheduler
        .run(Arc::new(AuthorityDrainJob::new("proj")))
        .await
        .unwrap_err();

    assert!(error.starts_with("storage:"), "{error}");
    scheduler.shutdown().await;
}
