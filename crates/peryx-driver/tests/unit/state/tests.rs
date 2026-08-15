use std::sync::Arc;

use peryx_core::Ecosystem;
use peryx_ha::{ReplicaPage, ReplicaViewApplier as _};
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;

use super::AppState;

struct BlockedView;

impl crate::serving::ReplicatedApplyDriver for BlockedView {
    fn apply_replicated_changes(
        &self,
        _state: &crate::ServingState,
        _changed_keys: &[String],
    ) -> Result<(), crate::state::ViewBlock> {
        Err(crate::state::ViewBlock {
            view: "search".to_owned(),
        })
    }
}

fn state() -> (tempfile::TempDir, AppState, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    meta.initialize_distributed_state().unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let state = AppState::new(meta.clone(), blobs, 60, Vec::new());
    (dir, state, meta)
}

#[test]
fn test_empty_replica_page_changes_nothing() {
    let (_dir, state, _meta) = state();

    state.apply(
        ReplicaPage {
            changes: 0,
            serial: 1,
            primary_serial: 1,
        },
        &[],
    );

    assert_eq!(peryx_ha::ReplicaViewApplier::readable_frontier(&state), 0);
}

#[test]
fn test_replica_page_advances_search_view() {
    let (_dir, state, meta) = state();

    state.apply(
        ReplicaPage {
            changes: 1,
            serial: 1,
            primary_serial: 1,
        },
        &["resource".to_owned()],
    );

    assert_eq!(meta.view_frontiers().unwrap().get(crate::state::SEARCH_VIEW), Some(&1));
}

#[test]
fn test_blocked_replica_view_does_not_advance_the_frontier() {
    let (_dir, mut state, meta) = state();
    state.register_replicated_apply_driver(Ecosystem::new("example"), Arc::new(BlockedView));

    state.apply(
        ReplicaPage {
            changes: 1,
            serial: 1,
            primary_serial: 1,
        },
        &["resource".to_owned()],
    );

    assert!(meta.view_frontiers().unwrap().is_empty());
}

#[test]
fn test_replica_apply_surfaces_a_frontier_write_failure_without_advancing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let meta = MetaStore::open(&path).unwrap();
    meta.initialize_distributed_state().unwrap();
    drop(meta);
    let state = AppState::new(
        MetaStore::open_existing_read_only(&path).unwrap(),
        BlobStore::new(dir.path().join("blobs")),
        60,
        Vec::new(),
    );

    state.apply(
        ReplicaPage {
            changes: 1,
            serial: 1,
            primary_serial: 1,
        },
        &[],
    );

    assert!(
        MetaStore::open_existing_read_only(&path)
            .unwrap()
            .view_frontiers()
            .unwrap()
            .is_empty()
    );
}
