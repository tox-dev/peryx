use std::sync::Arc;

use async_trait::async_trait;
use peryx_driver::AppState;
use peryx_ha::{
    AnalyticsCompleteness, AnalyticsSnapshotStore, AuthorityEpoch, BlobDurability, BlobServices, BlobWriteDurability,
    CommittedBlob, CompletenessError, CompletenessQuery, CompletenessReport, Digest, ExpectedProducer,
    ReplicaViewApplier as _, WriteDurability,
};
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;

#[tokio::test]
async fn distributed_frontier_publishes_to_current_and_late_observers() {
    let (_dir, mut state) = state();
    state
        .install_distributed_availability(peryx_ha::AvailabilityStateInstall {
            role: peryx_core::NodeRole::Replica,
            topology: peryx_core::TopologyConfig::default(),
            blobs: BlobServices::new(None, Arc::new(UnavailableDurability)),
            analytics: Arc::new(UnavailableCompleteness),
            capabilities: peryx_ha::AvailabilityCapabilities::default(),
            authority_drainer: None,
            operations: None,
        })
        .unwrap();
    let mut current = state.serving.replica_applied_frontier().unwrap();
    assert_eq!(*current.borrow_and_update(), 0);

    state.publish_applied_frontier(41);

    assert!(current.has_changed().unwrap());
    assert_eq!(*current.borrow_and_update(), 41);
    assert_eq!(*state.serving.replica_applied_frontier().unwrap().borrow(), 41);
    let digest = Digest::of(b"frontier");
    assert_eq!(
        state
            .serving
            .confirm_blob_write(CommittedBlob::new(
                &digest,
                b"frontier".len() as u64,
                "repository",
                AuthorityEpoch(1),
                None,
                BlobDurability::Filesystem,
            ))
            .await,
        WriteDurability::Unavailable
    );
    assert!(
        state
            .serving
            .analytics_completeness()
            .unwrap()
            .assess(
                &state.serving.meta,
                &[],
                &CompletenessQuery {
                    from_day: 1,
                    to_day: 1,
                    today: 1,
                    repository: None,
                },
            )
            .is_err()
    );
}

#[test]
fn local_frontier_has_no_observer_and_ignores_publication() {
    let (_dir, state) = state();
    assert!(state.serving.replica_applied_frontier().is_none());

    state.publish_applied_frontier(41);

    assert!(state.serving.replica_applied_frontier().is_none());
}

fn state() -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    (dir, AppState::new(meta, blobs, 60, Vec::new()))
}

struct UnavailableDurability;

#[async_trait]
impl BlobWriteDurability for UnavailableDurability {
    async fn confirm(&self, _write: CommittedBlob<'_>) -> WriteDurability {
        WriteDurability::Unavailable
    }
}

struct UnavailableCompleteness;

impl AnalyticsCompleteness for UnavailableCompleteness {
    fn assess(
        &self,
        _store: &dyn AnalyticsSnapshotStore,
        _expected: &[ExpectedProducer],
        _query: &CompletenessQuery,
    ) -> Result<CompletenessReport, CompletenessError> {
        Err(CompletenessError)
    }
}
