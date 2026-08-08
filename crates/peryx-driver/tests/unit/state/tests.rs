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

fn state() -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    (dir, AppState::new(meta, blobs, 60, Vec::new()))
}

#[test]
fn test_empty_replica_page_changes_nothing() {
    let (_dir, state) = state();

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
    let (_dir, state) = state();

    state.apply(
        ReplicaPage {
            changes: 1,
            serial: 1,
            primary_serial: 1,
        },
        &["project".to_owned()],
    );

    assert_eq!(
        state.meta.view_frontiers().unwrap().get(crate::state::SEARCH_VIEW),
        Some(&1)
    );
}

#[test]
fn test_blocked_replica_view_does_not_advance_the_frontier() {
    let (_dir, mut state) = state();
    state.register_replicated_apply_driver(Ecosystem::new("example"), Arc::new(BlockedView));

    state.apply(
        ReplicaPage {
            changes: 1,
            serial: 1,
            primary_serial: 1,
        },
        &["project".to_owned()],
    );

    assert!(state.meta.view_frontiers().unwrap().is_empty());
}

#[test]
fn test_replica_apply_surfaces_a_frontier_write_failure_without_advancing() {
    let fault = peryx_storage::meta::test_support::FaultStore::new();
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(fault.reopen(), BlobStore::new(dir.path().join("blobs")), 60, Vec::new());
    fault.arm(0);

    state.apply(
        ReplicaPage {
            changes: 1,
            serial: 1,
            primary_serial: 1,
        },
        &[],
    );

    fault.disable();
    assert!(fault.reopen().view_frontiers().unwrap().is_empty());
}
