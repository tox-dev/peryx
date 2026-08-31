use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use peryx_core::{NodeRole, TopologyConfig, TopologyMember, TopologyMode};
use peryx_ha::{AuthorityEpoch, OperationKind, OperationObservation, OperationObserver};
use peryx_storage::blob::{BlobStore, Digest, WriteEvidence};
use peryx_storage::meta::MetaStore;

use crate::state::{AppState, ServingState};

fn state_with(topology: TopologyConfig) -> (tempfile::TempDir, Arc<ServingState>, Arc<Capture>) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let mut state = AppState::new(meta, blobs, 60, Vec::new());
    let capture = Arc::new(Capture::default());
    state
        .install_distributed_availability(peryx_ha::AvailabilityStateInstall {
            role: NodeRole::Writer,
            topology,
            blobs: peryx_ha::BlobServices::new(None, Arc::new(Durability)),
            analytics: Arc::new(Completeness),
            capabilities: peryx_ha::AvailabilityCapabilities::default(),
            authority_drainer: None,
            operations: Some(capture.clone()),
        })
        .unwrap();
    (dir, state.serving, capture)
}

fn topology(local: Option<&str>) -> TopologyConfig {
    TopologyConfig {
        mode: TopologyMode::Dc,
        group: Some("group".to_owned()),
        members: vec![TopologyMember {
            node: "writer".to_owned(),
            dc: "east-1".to_owned(),
            address: "writer:8080".to_owned(),
            role: NodeRole::Writer,
        }],
        local_node: local.map(ToOwned::to_owned),
    }
}

#[derive(Default)]
struct Capture(Mutex<Vec<OperationObservation>>);

impl OperationObserver for Capture {
    fn record(&self, operation: OperationObservation) {
        self.0.lock().unwrap().push(operation);
    }
}

struct Durability;

#[async_trait]
impl peryx_ha::BlobWriteDurability for Durability {
    async fn confirm(&self, _write: peryx_ha::CommittedBlob<'_>) -> peryx_ha::WriteDurability {
        peryx_ha::WriteDurability::Unavailable
    }
}

struct Completeness;

impl peryx_ha::AnalyticsCompleteness for Completeness {
    fn assess(
        &self,
        _meta: &dyn peryx_ha::AnalyticsSnapshotStore,
        _expected: &[peryx_ha::ExpectedProducer],
        _query: &peryx_ha::CompletenessQuery,
    ) -> Result<peryx_ha::CompletenessReport, peryx_ha::CompletenessError> {
        Err(peryx_ha::CompletenessError)
    }
}

#[tokio::test]
async fn test_trace_state_reports_write_durability_unavailable() {
    let (_dir, state, _capture) = state_with(topology(Some("writer")));
    let digest = Digest::of(b"artifact");

    assert_eq!(
        state
            .confirm_blob_write(peryx_ha::CommittedBlob::new(
                &digest,
                b"artifact".len() as u64,
                "catalog",
                AuthorityEpoch(4),
                None,
                WriteEvidence::NodeLocal,
            ))
            .await,
        peryx_ha::WriteDurability::Unavailable
    );
}

#[test]
fn test_trace_state_reports_analytics_completeness_unavailable() {
    let (_dir, state, _capture) = state_with(topology(Some("writer")));

    assert!(
        state
            .analytics_completeness()
            .unwrap()
            .assess(
                &state.meta,
                &[],
                &peryx_ha::CompletenessQuery {
                    from_day: 1,
                    to_day: 2,
                    today: 3,
                    repository: Some("catalog".to_owned()),
                },
            )
            .is_err()
    );
}

#[test]
fn test_record_operation_trace_forwards_the_committed_identity() {
    let (_dir, state, capture) = state_with(topology(Some("writer")));
    for _ in 0..3 {
        state.meta.next_serial().unwrap();
    }

    state.record_operation_trace(OperationKind::Publish, 4);

    assert_eq!(
        *capture.0.lock().unwrap(),
        vec![OperationObservation {
            source: "writer".to_owned(),
            epoch: AuthorityEpoch(4),
            serial: 3,
            kind: OperationKind::Publish,
        }]
    );
}

#[test]
fn test_record_operation_trace_uses_the_standalone_source_without_a_local_member() {
    let (_dir, state, capture) = state_with(topology(None));

    state.record_operation_trace(OperationKind::Delete, 7);

    assert_eq!(capture.0.lock().unwrap()[0].source, "standalone");
}

#[test]
fn test_record_operation_trace_is_inert_without_availability() {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(
        MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        BlobStore::new(dir.path().join("blobs")),
        60,
        Vec::new(),
    )
    .serving;

    state.record_operation_trace(OperationKind::Publish, 1);

    assert_eq!(state.meta.current_serial().unwrap(), 0);
}

#[test]
fn test_record_operation_trace_is_inert_without_observer() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = AppState::new(
        MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        BlobStore::new(dir.path().join("blobs")),
        60,
        Vec::new(),
    );
    state
        .install_distributed_availability(peryx_ha::AvailabilityStateInstall {
            role: NodeRole::Writer,
            topology: topology(Some("writer")),
            blobs: peryx_ha::BlobServices::new(None, Arc::new(Durability)),
            analytics: Arc::new(Completeness),
            capabilities: peryx_ha::AvailabilityCapabilities::default(),
            authority_drainer: None,
            operations: None,
        })
        .unwrap();
    let state = state.serving;

    state.record_operation_trace(OperationKind::Publish, 1);

    assert_eq!(state.meta.current_serial().unwrap(), 0);
}
