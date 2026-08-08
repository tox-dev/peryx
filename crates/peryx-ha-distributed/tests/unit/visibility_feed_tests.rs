use std::cell::{Cell, RefCell};
use std::io;

use peryx_storage::meta::MetaStore;
use tempfile::tempdir;

use crate::envelope::{AuthorityEpoch, OperationEnvelope, OperationKind};
use crate::protocol::Change;
use crate::visibility::VisibilityAction::{Lift, Restore, Revoke, Trash};
use crate::visibility::{
    ApplyEffect, ArtifactId, Frontier as VisibilityFrontier, OpOrder, VisibilityAction, VisibilityOp,
};
use crate::visibility_feed::{
    ApplyEnvelopeError, OpenError, VisibilityFeedError, VisibilityProjection, VisibilitySnapshotStore,
    decode_visibility_op, visibility_change, visibility_envelope,
};

#[derive(Default)]
struct MemStore {
    data: RefCell<Option<Vec<u8>>>,
    saves: Cell<usize>,
    fail_save: Cell<bool>,
    fail_load: Cell<bool>,
}

impl VisibilitySnapshotStore for MemStore {
    type Error = io::Error;

    fn load_snapshot(&self) -> Result<Option<Vec<u8>>, io::Error> {
        if self.fail_load.get() {
            return Err(io::Error::other("load failed"));
        }
        Ok(self.data.borrow().clone())
    }

    fn save_snapshot(&self, bytes: &[u8]) -> Result<(), io::Error> {
        if self.fail_save.get() {
            return Err(io::Error::other("save failed"));
        }
        self.saves.set(self.saves.get() + 1);
        *self.data.borrow_mut() = Some(bytes.to_vec());
        Ok(())
    }
}

fn artifact(coordinate: &str) -> ArtifactId {
    ArtifactId {
        coordinate: coordinate.to_owned(),
        digest: "sha256:deadbeef".to_owned(),
    }
}

fn op(coordinate: &str, action: VisibilityAction, epoch: u64, serial: u64) -> VisibilityOp {
    VisibilityOp {
        artifact: artifact(coordinate),
        action,
        order: OpOrder { epoch, serial },
    }
}

#[test]
fn test_visibility_change_carries_the_operation_serial_without_row_or_blob_changes() {
    let change = visibility_change(&op("root/alpha/flask/2.0.0", Trash, 3, 9));
    assert_eq!(change.serial, 9);
    assert!(change.metadata.is_empty());
    assert!(change.blobs.is_empty());
    assert!(!change.event.is_empty());
}

#[test]
fn test_visibility_envelope_tags_the_kind_and_epoch_and_round_trips_the_operation() {
    let minted = op("root/alpha/flask/2.0.0", Revoke, 4, 12);
    let envelope = visibility_envelope("dc-a", &minted);
    assert_eq!(envelope.kind, OperationKind::Visibility);
    assert_eq!(envelope.epoch, AuthorityEpoch(4));
    let decoded = OperationEnvelope::decode(&envelope.encode(), crate::DEFAULT_DECODE_LIMITS).unwrap();
    assert_eq!(decode_visibility_op(&decoded).unwrap(), Some(minted));
}

#[test]
fn test_decode_ignores_an_envelope_of_another_kind() {
    let change = visibility_change(&op("root/alpha/flask/2.0.0", Trash, 1, 1));
    let envelope = OperationEnvelope::current("dc-a", AuthorityEpoch(1), OperationKind::Publish, change);
    assert_eq!(decode_visibility_op(&envelope).unwrap(), None);
}

#[test]
fn test_decode_rejects_a_malformed_visibility_payload() {
    let envelope = OperationEnvelope::current(
        "dc-a",
        AuthorityEpoch(1),
        OperationKind::Visibility,
        Change {
            serial: 1,
            event: b"not json".to_vec(),
            metadata: Vec::new(),
            blobs: Vec::new(),
        },
    );
    assert!(matches!(
        decode_visibility_op(&envelope),
        Err(VisibilityFeedError::Malformed(_))
    ));
}

#[test]
fn test_decode_rejects_a_payload_schema_this_build_does_not_apply() {
    let event = serde_json::to_vec(&serde_json::json!({
        "schema": 2,
        "artifact": {"coordinate": "root/alpha/flask/2.0.0", "digest": "sha256:deadbeef"},
        "action": "trash",
        "order": {"epoch": 1, "serial": 1},
    }))
    .unwrap();
    let envelope = OperationEnvelope::current(
        "dc-a",
        AuthorityEpoch(1),
        OperationKind::Visibility,
        Change {
            serial: 1,
            event,
            metadata: Vec::new(),
            blobs: Vec::new(),
        },
    );
    assert!(matches!(
        decode_visibility_op(&envelope),
        Err(VisibilityFeedError::UnsupportedSchema { expected: 1, found: 2 })
    ));
}

#[test]
fn test_decode_rejects_a_serial_that_disagrees_with_the_envelope() {
    let mut change = visibility_change(&op("root/alpha/flask/2.0.0", Trash, 7, 7));
    change.serial = 5;
    let envelope = OperationEnvelope::current("dc-a", AuthorityEpoch(7), OperationKind::Visibility, change);
    assert!(matches!(
        decode_visibility_op(&envelope),
        Err(VisibilityFeedError::IdentityMismatch {
            envelope_serial: 5,
            op_serial: 7,
            ..
        })
    ));
}

#[test]
fn test_decode_rejects_an_epoch_that_disagrees_with_the_envelope() {
    let change = visibility_change(&op("root/alpha/flask/2.0.0", Trash, 9, 5));
    let envelope = OperationEnvelope::current("dc-a", AuthorityEpoch(1), OperationKind::Visibility, change);
    assert!(matches!(
        decode_visibility_op(&envelope),
        Err(VisibilityFeedError::IdentityMismatch {
            envelope_epoch: 1,
            op_epoch: 9,
            ..
        })
    ));
}

#[test]
fn test_open_on_an_empty_store_serves_the_visible_default() {
    let store = MemStore::default();
    let projection = VisibilityProjection::open(&store).unwrap();
    assert_eq!(projection.retained_artifacts(), 0);
    assert!(projection.visibility(&artifact("root/alpha/flask/2.0.0")).is_visible());
    assert!(projection.advertised().high_water(1).is_none());
}

#[test]
fn test_apply_folds_a_batch_persists_once_and_advertises_the_frontier() {
    let store = MemStore::default();
    let mut projection = VisibilityProjection::open(&store).unwrap();
    let effects = projection
        .apply(&[op("a", Trash, 1, 1), op("b", Revoke, 1, 2)])
        .unwrap();
    assert_eq!(effects, vec![ApplyEffect::Applied, ApplyEffect::Applied]);
    assert!(projection.visibility(&artifact("a")).trashed);
    assert!(projection.visibility(&artifact("b")).revoked);
    assert_eq!(projection.advertised().high_water(1), Some(2));
    assert_eq!(store.saves.get(), 1);
}

#[test]
fn test_a_batch_that_changes_nothing_skips_the_save() {
    let store = MemStore::default();
    let mut projection = VisibilityProjection::open(&store).unwrap();
    projection.apply(&[op("a", Revoke, 1, 10)]).unwrap();
    let effects = projection.apply(&[op("a", Lift, 1, 5)]).unwrap();
    assert_eq!(effects, vec![ApplyEffect::Ignored]);
    assert!(projection.visibility(&artifact("a")).revoked);
    assert_eq!(store.saves.get(), 1);
}

#[test]
fn test_an_empty_batch_neither_saves_nor_reports_effects() {
    let store = MemStore::default();
    let mut projection = VisibilityProjection::open(&store).unwrap();
    assert!(projection.apply(&[]).unwrap().is_empty());
    assert_eq!(store.saves.get(), 0);
}

#[test]
fn test_out_of_order_delivery_cannot_resurrect_a_revoked_artifact() {
    let store = MemStore::default();
    let mut projection = VisibilityProjection::open(&store).unwrap();
    projection.apply(&[op("a", Revoke, 1, 10)]).unwrap();
    projection.apply(&[op("a", Lift, 1, 5)]).unwrap();
    assert!(projection.visibility(&artifact("a")).revoked);
}

#[test]
fn test_a_late_lower_epoch_lift_cannot_undo_a_failover_revoke_but_still_advertises() {
    let store = MemStore::default();
    let mut projection = VisibilityProjection::open(&store).unwrap();
    projection.apply(&[op("a", Lift, 1, 5)]).unwrap();
    projection.apply(&[op("a", Revoke, 2, 1)]).unwrap();
    let effects = projection.apply(&[op("a", Lift, 1, 6)]).unwrap();
    assert_eq!(effects, vec![ApplyEffect::Ignored]);
    assert!(projection.visibility(&artifact("a")).revoked);
    assert_eq!(projection.advertised().high_water(1), Some(6));
    assert_eq!(projection.advertised().high_water(2), Some(1));
}

#[test]
fn test_an_applied_operation_below_the_epoch_high_water_still_persists() {
    let store = MemStore::default();
    let mut projection = VisibilityProjection::open(&store).unwrap();
    projection.apply(&[op("a", Trash, 1, 5)]).unwrap();
    let effects = projection.apply(&[op("a", Revoke, 1, 3)]).unwrap();
    assert_eq!(effects, vec![ApplyEffect::Applied]);
    assert!(projection.visibility(&artifact("a")).revoked);
    assert_eq!(projection.advertised().high_water(1), Some(5));
    assert_eq!(store.saves.get(), 2);
}

#[test]
fn test_a_save_failure_leaves_the_projection_and_advertised_frontier_unchanged() {
    let store = MemStore::default();
    let mut projection = VisibilityProjection::open(&store).unwrap();
    projection.apply(&[op("a", Trash, 1, 1)]).unwrap();
    store.fail_save.set(true);
    let error = projection.apply(&[op("a", Revoke, 1, 2)]).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Other);
    assert!(projection.visibility(&artifact("a")).trashed);
    assert!(!projection.visibility(&artifact("a")).revoked);
    assert_eq!(projection.advertised().high_water(1), Some(1));
}

#[test]
fn test_apply_envelopes_routes_visibility_operations_and_ignores_other_kinds() {
    let store = MemStore::default();
    let mut projection = VisibilityProjection::open(&store).unwrap();
    let upload = OperationEnvelope::current(
        "dc-a",
        AuthorityEpoch(1),
        OperationKind::Publish,
        visibility_change(&op("noise", Trash, 1, 1)),
    );
    let trash = visibility_envelope("dc-a", &op("a", Trash, 1, 2));
    let effects = projection.apply_envelopes(&[upload, trash]).unwrap();
    assert_eq!(effects, vec![ApplyEffect::Applied]);
    assert!(projection.visibility(&artifact("a")).trashed);
    assert!(projection.visibility(&artifact("noise")).is_visible());
}

#[test]
fn test_apply_envelopes_surfaces_a_decode_failure() {
    let store = MemStore::default();
    let mut projection = VisibilityProjection::open(&store).unwrap();
    let malformed = OperationEnvelope::current(
        "dc-a",
        AuthorityEpoch(1),
        OperationKind::Visibility,
        Change {
            serial: 1,
            event: b"not json".to_vec(),
            metadata: Vec::new(),
            blobs: Vec::new(),
        },
    );
    assert!(matches!(
        projection.apply_envelopes(&[malformed]),
        Err(ApplyEnvelopeError::Decode(VisibilityFeedError::Malformed(_)))
    ));
}

#[test]
fn test_apply_envelopes_surfaces_a_persistence_failure() {
    let store = MemStore::default();
    let mut projection = VisibilityProjection::open(&store).unwrap();
    store.fail_save.set(true);
    let trash = visibility_envelope("dc-a", &op("a", Trash, 1, 1));
    assert!(matches!(
        projection.apply_envelopes(&[trash]),
        Err(ApplyEnvelopeError::Store(_))
    ));
}

#[test]
fn test_compaction_releases_a_settled_visible_artifact_and_keeps_a_tombstone() {
    let store = MemStore::default();
    let mut projection = VisibilityProjection::open(&store).unwrap();
    projection
        .apply(&[
            op("visible", Trash, 1, 1),
            op("visible", Restore, 1, 2),
            op("tombstone", Revoke, 1, 3),
        ])
        .unwrap();
    let mut frontier = VisibilityFrontier::default();
    frontier.acknowledge(1, 3);
    projection.compact(&frontier).unwrap();
    assert_eq!(projection.retained_artifacts(), 1);
    assert!(projection.visibility(&artifact("visible")).is_visible());
    assert!(projection.visibility(&artifact("tombstone")).revoked);
}

#[test]
fn test_compaction_persistence_failure_leaves_the_projection_unchanged() {
    let store = MemStore::default();
    let mut projection = VisibilityProjection::open(&store).unwrap();
    projection
        .apply(&[op("a", Trash, 1, 1), op("a", Restore, 1, 2)])
        .unwrap();
    store.fail_save.set(true);
    let mut frontier = VisibilityFrontier::default();
    frontier.acknowledge(1, 2);
    assert!(projection.compact(&frontier).is_err());
    assert_eq!(projection.retained_artifacts(), 1);
}

#[test]
fn test_a_reopened_projection_recovers_tombstones_and_the_advertised_frontier() {
    let store = MemStore::default();
    {
        let mut projection = VisibilityProjection::open(&store).unwrap();
        projection
            .apply(&[op("a", Trash, 1, 1), op("b", Revoke, 2, 4)])
            .unwrap();
    }
    let projection = VisibilityProjection::open(&store).unwrap();
    assert!(projection.visibility(&artifact("a")).trashed);
    assert!(projection.visibility(&artifact("b")).revoked);
    assert_eq!(projection.advertised().high_water(1), Some(1));
    assert_eq!(projection.advertised().high_water(2), Some(4));
}

#[test]
fn test_open_surfaces_a_snapshot_load_failure() {
    let store = MemStore::default();
    store.fail_load.set(true);
    assert!(matches!(VisibilityProjection::open(&store), Err(OpenError::Store(_))));
}

#[test]
fn test_open_rejects_a_malformed_snapshot() {
    let store = MemStore::default();
    *store.data.borrow_mut() = Some(b"not json".to_vec());
    assert!(matches!(
        VisibilityProjection::open(&store),
        Err(OpenError::Malformed(_))
    ));
}

#[test]
fn test_open_rejects_a_snapshot_schema_this_build_does_not_restore() {
    let store = MemStore::default();
    *store.data.borrow_mut() = Some(
        serde_json::to_vec(&serde_json::json!({
            "schema": 2,
            "advertised": {"covered": {}},
            "state": [],
        }))
        .unwrap(),
    );
    assert!(matches!(
        VisibilityProjection::open(&store),
        Err(OpenError::UnsupportedSchema { expected: 1, found: 2 })
    ));
}

#[test]
fn test_open_rejects_an_unrestorable_apply_state() {
    let store = MemStore::default();
    *store.data.borrow_mut() = Some(
        serde_json::to_vec(&serde_json::json!({
            "schema": 1,
            "advertised": {"covered": {}},
            "state": b"garbage".to_vec(),
        }))
        .unwrap(),
    );
    assert!(matches!(VisibilityProjection::open(&store), Err(OpenError::State(_))));
}

#[test]
fn test_the_metadata_store_carries_the_projection_across_a_restart() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    {
        let store = MetaStore::open(&path).unwrap();
        let mut projection = VisibilityProjection::open(store).unwrap();
        projection.apply(&[op("a", Revoke, 1, 1)]).unwrap();
    }
    let store = MetaStore::open(&path).unwrap();
    let projection = VisibilityProjection::open(store).unwrap();
    assert!(projection.visibility(&artifact("a")).revoked);
    assert_eq!(projection.advertised().high_water(1), Some(1));
}
