use std::sync::Arc;

use peryx_core::{BlobPlacementStatus, UiArtifactSource, UiByteAvailability, UiOperationStatus};
use peryx_ha::{
    ArtifactPlacement, ArtifactSource, AvailabilityPageQuery, AvailabilityViewReader, BackendId, BackendLocation,
    BlobPlacementKey, BlobPlacementRecord, BlobPlacementState, BlobPlacementStore, CompareWrite, DataCenterId,
    OperationsViewError, PlacementViewError,
};
use peryx_identity::ArtifactDigest;
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::{MetaStore, OperationResult};

use crate::AppState;

fn state() -> (tempfile::TempDir, AppState) {
    let directory = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(directory.path().join("peryx.redb")).unwrap();
    meta.initialize_distributed_state().unwrap();
    let blobs = BlobStore::new(directory.path().join("blobs"));
    (
        directory,
        AppState::with_clock(meta, blobs, 60, Vec::new(), Arc::new(|| 10)),
    )
}

fn query(include_rows: bool, limit: usize) -> AvailabilityPageQuery {
    AvailabilityPageQuery {
        cursor: None,
        limit,
        include_rows,
    }
}

#[test]
fn placement_view_projects_health_rows_and_paging() {
    let (_directory, state) = state();
    for (digest, source, present) in [
        ("sha256:1", ArtifactSource::Hosted, true),
        ("sha256:2", ArtifactSource::Proxy, false),
        ("sha256:3", ArtifactSource::Generated, false),
    ] {
        state
            .serving
            .meta
            .put_artifact_placement(digest, &ArtifactPlacement::record(source, present))
            .unwrap();
    }

    let health = state.serving.placement_view(query(false, 0)).unwrap();
    assert_eq!(
        (
            health.health.local,
            health.health.remote_only,
            health.health.unavailable
        ),
        (1, 1, 1)
    );
    assert_eq!(health.health.total, 3);
    assert!(health.rows.is_none());

    let first = state.serving.placement_view(query(true, 2)).unwrap();
    assert_eq!(first.next_cursor.as_deref(), Some("sha256:2"));
    let rows = first.rows.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        (rows[0].source, rows[0].availability),
        (UiArtifactSource::Hosted, UiByteAvailability::Local)
    );
    assert_eq!(
        (rows[1].source, rows[1].availability),
        (UiArtifactSource::Proxy, UiByteAvailability::RemoteOnly)
    );

    let second = state
        .serving
        .placement_view(AvailabilityPageQuery {
            cursor: first.next_cursor,
            limit: 2,
            include_rows: true,
        })
        .unwrap();
    let row = &second.rows.unwrap()[0];
    assert_eq!(
        (row.source, row.availability),
        (UiArtifactSource::Generated, UiByteAvailability::Unavailable)
    );
    assert!(second.next_cursor.is_none());
}

#[test]
fn placement_view_rejects_invalid_limits() {
    let (_directory, state) = state();
    assert_eq!(
        state.serving.placement_view(query(true, 0)),
        Err(PlacementViewError::InvalidLimit)
    );
}

#[test]
fn blob_placement_view_projects_and_orders_states() {
    let (_directory, state) = state();
    let digest = ArtifactDigest::from_sha256("a".repeat(64)).unwrap();
    for (data_center, state_value, updated_at) in [
        ("west", BlobPlacementState::Pending, 4),
        ("east", BlobPlacementState::Verified { size: 9 }, 3),
        ("north", BlobPlacementState::Revoked, 2),
        (
            "south",
            BlobPlacementState::Failed {
                class: peryx_ha::BlobPlacementFailure::SourceUnavailable,
            },
            1,
        ),
    ] {
        let record = BlobPlacementRecord {
            key: BlobPlacementKey {
                digest: digest.clone(),
                backend: BackendId::new(format!("backend-{data_center}")).unwrap(),
                data_center: DataCenterId::new(data_center).unwrap(),
                location: BackendLocation::new(format!("location-{data_center}")).unwrap(),
            },
            state: state_value,
            fence: 1,
            transfer_attempt: 1,
            generation: 1,
            updated_at_unix: updated_at,
        };
        assert_eq!(
            BlobPlacementStore::compare_and_put_blob_placement(&state.serving.meta, None, &record).unwrap(),
            CompareWrite::Written
        );
    }

    let view = state.serving.blob_placement_view(&digest.canonical()).unwrap();
    assert_eq!(view.digest, digest.canonical());
    assert_eq!(
        view.datacenters
            .iter()
            .map(|row| (row.data_center.as_str(), row.status, row.size))
            .collect::<Vec<_>>(),
        vec![
            ("east", BlobPlacementStatus::Verified, Some(9)),
            ("north", BlobPlacementStatus::Revoked, None),
            ("south", BlobPlacementStatus::Failed, None),
            ("west", BlobPlacementStatus::Pending, None),
        ]
    );
}

#[test]
fn blob_placement_view_rejects_invalid_digests() {
    let (_directory, state) = state();
    assert_eq!(
        state.serving.blob_placement_view("invalid"),
        Err(peryx_ha::BlobPlacementViewError::InvalidDigest)
    );
}

#[test]
fn operations_view_projects_health_rows_and_paging() {
    let (_directory, state) = state();
    for (operation, expiry) in [
        ("pending", None),
        ("expired", Some(9)),
        ("published", None),
        ("failed", None),
    ] {
        state.serving.meta.claim_operation(operation, expiry, 1).unwrap();
    }
    state
        .serving
        .meta
        .finalize_operation("published", OperationResult::Published, b"ok", 2)
        .unwrap();
    state
        .serving
        .meta
        .finalize_operation("failed", OperationResult::Failed, b"error", 2)
        .unwrap();

    let health = state.serving.operations_view(query(false, 0)).unwrap();
    assert_eq!(
        (
            health.health.pending,
            health.health.published,
            health.health.failed,
            health.health.expired,
            health.health.total,
        ),
        (1, 1, 1, 1, 4)
    );
    assert!(health.rows.is_none());

    let first = state.serving.operations_view(query(true, 2)).unwrap();
    assert!(first.next_cursor.is_some());
    let second = state
        .serving
        .operations_view(AvailabilityPageQuery {
            cursor: first.next_cursor,
            limit: 2,
            include_rows: true,
        })
        .unwrap();
    let statuses = first
        .rows
        .unwrap()
        .into_iter()
        .chain(second.rows.unwrap())
        .map(|row| row.status)
        .collect::<Vec<_>>();
    assert!(statuses.contains(&UiOperationStatus::Pending));
    assert!(statuses.contains(&UiOperationStatus::Expired));
    assert!(statuses.contains(&UiOperationStatus::Published));
    assert!(statuses.contains(&UiOperationStatus::Failed));
    assert!(second.next_cursor.is_none());
}

#[test]
fn operations_view_rejects_invalid_limits() {
    let (_directory, state) = state();
    assert_eq!(
        state.serving.operations_view(query(true, 0)),
        Err(OperationsViewError::InvalidLimit)
    );
}

fn corrupt(table: &'static str) -> (tempfile::TempDir, AppState) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("peryx.redb");
    let meta = MetaStore::open(&path).unwrap();
    meta.initialize_distributed_state().unwrap();
    drop(meta);
    let database = redb::Database::open(&path).unwrap();
    let transaction = database.begin_write().unwrap();
    transaction
        .delete_table(redb::TableDefinition::<&str, &[u8]>::new(table))
        .unwrap();
    transaction
        .open_table(redb::TableDefinition::<&str, u64>::new(table))
        .unwrap();
    transaction.commit().unwrap();
    drop(database);
    let meta = MetaStore::open_existing(path).unwrap();
    let blobs = BlobStore::new(directory.path().join("blobs"));
    (
        directory,
        AppState::with_clock(meta, blobs, 60, Vec::new(), Arc::new(|| 10)),
    )
}

#[test]
fn view_errors_identify_the_failed_read() {
    let (_directory, state) = corrupt("artifact_placement");
    assert_eq!(
        state.serving.placement_view(query(false, 25)),
        Err(PlacementViewError::HealthRead)
    );
    assert_eq!(
        state.serving.placement_view(query(true, 25)),
        Err(PlacementViewError::RowsRead)
    );

    let (_directory, state) = corrupt("blob_placement");
    let digest = ArtifactDigest::from_sha256("a".repeat(64)).unwrap();
    assert_eq!(
        state.serving.blob_placement_view(&digest.canonical()),
        Err(peryx_ha::BlobPlacementViewError::Read)
    );

    let (_directory, state) = corrupt("operation_outcome");
    assert_eq!(
        state.serving.operations_view(query(false, 25)),
        Err(OperationsViewError::HealthRead)
    );
    assert_eq!(
        state.serving.operations_view(query(true, 25)),
        Err(OperationsViewError::RowsRead)
    );
}
