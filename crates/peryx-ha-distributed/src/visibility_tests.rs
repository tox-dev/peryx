use crate::visibility::VisibilityAction::{Lift, Restore, Revoke, Trash};
use crate::visibility::{
    ApplyEffect, ArtifactId, Frontier, OpOrder, SnapshotError, VISIBILITY_APPLY_SCHEMA, Visibility, VisibilityAction,
    VisibilityOp, VisibilityState,
};

fn frontier(epoch: u64, serial: u64) -> Frontier {
    let mut frontier = Frontier::default();
    frontier.acknowledge(epoch, serial);
    frontier
}

fn artifact(coordinate: &str) -> ArtifactId {
    ArtifactId {
        coordinate: coordinate.to_owned(),
        digest: "sha256:deadbeef".to_owned(),
    }
}

fn op(artifact: &ArtifactId, action: VisibilityAction, epoch: u64, serial: u64) -> VisibilityOp {
    VisibilityOp {
        artifact: artifact.clone(),
        action,
        order: OpOrder { epoch, serial },
    }
}

#[test]
fn test_trash_then_restore_toggles_visibility() {
    let mut state = VisibilityState::new();
    let art = artifact("root/pypi/flask/2.0.0");

    assert_eq!(state.apply(&op(&art, Trash, 1, 5)), ApplyEffect::Applied);
    assert_eq!(
        state.get(&art),
        Visibility {
            trashed: true,
            revoked: false
        }
    );
    assert!(!state.get(&art).is_visible());

    assert_eq!(state.apply(&op(&art, Restore, 1, 6)), ApplyEffect::Applied);
    assert!(state.get(&art).is_visible());
}

#[test]
fn test_revoke_then_lift_toggles_visibility() {
    let mut state = VisibilityState::new();
    let art = artifact("root/oci/nginx@sha256:deadbeef");

    assert_eq!(state.apply(&op(&art, Revoke, 1, 5)), ApplyEffect::Applied);
    assert_eq!(
        state.get(&art),
        Visibility {
            trashed: false,
            revoked: true
        }
    );
    assert!(!state.get(&art).is_visible());

    assert_eq!(state.apply(&op(&art, Lift, 1, 6)), ApplyEffect::Applied);
    assert!(state.get(&art).is_visible());
}

#[test]
fn test_duplicate_apply_is_a_no_op() {
    let mut state = VisibilityState::new();
    let art = artifact("a");

    assert_eq!(state.apply(&op(&art, Trash, 1, 5)), ApplyEffect::Applied);
    assert_eq!(state.apply(&op(&art, Trash, 1, 5)), ApplyEffect::Ignored);
    assert!(state.get(&art).trashed);
}

#[test]
fn test_stale_reordered_op_does_not_resurrect() {
    let mut state = VisibilityState::new();
    let art = artifact("a");

    assert_eq!(state.apply(&op(&art, Trash, 1, 5)), ApplyEffect::Applied);
    assert_eq!(state.apply(&op(&art, Restore, 1, 4)), ApplyEffect::Ignored);
    assert!(
        state.get(&art).trashed,
        "a stale restore must not resurrect the artifact"
    );
    assert!(!state.get(&art).is_visible());
}

#[test]
fn test_lower_epoch_op_never_overwrites_but_higher_epoch_wins() {
    let mut state = VisibilityState::new();
    let art = artifact("a");

    assert_eq!(state.apply(&op(&art, Revoke, 1, 5)), ApplyEffect::Applied);
    assert_eq!(state.apply(&op(&art, Lift, 0, 9)), ApplyEffect::Ignored);
    assert!(
        state.get(&art).revoked,
        "a lower-epoch lift must not overwrite despite a higher serial"
    );

    assert_eq!(state.apply(&op(&art, Lift, 2, 1)), ApplyEffect::Applied);
    assert!(
        !state.get(&art).revoked,
        "a higher-epoch lift wins despite a lower serial"
    );
}

#[test]
fn test_trash_and_revoke_are_independent_dimensions() {
    let mut state = VisibilityState::new();
    let art = artifact("a");

    assert_eq!(state.apply(&op(&art, Revoke, 1, 6)), ApplyEffect::Applied);
    assert_eq!(state.apply(&op(&art, Trash, 1, 5)), ApplyEffect::Applied);

    assert_eq!(
        state.get(&art),
        Visibility {
            trashed: true,
            revoked: true
        }
    );
}

#[test]
fn test_unknown_artifact_is_visible() {
    let state = VisibilityState::new();

    assert_eq!(state.get(&artifact("never-touched")), Visibility::default());
    assert!(state.get(&artifact("never-touched")).is_visible());
}

#[test]
fn test_advancing_the_high_water_blocks_an_intermediate_op() {
    let mut state = VisibilityState::new();
    let art = artifact("a");

    assert_eq!(state.apply(&op(&art, Trash, 1, 5)), ApplyEffect::Applied);
    assert_eq!(state.apply(&op(&art, Trash, 1, 7)), ApplyEffect::Applied);
    assert_eq!(state.apply(&op(&art, Restore, 1, 6)), ApplyEffect::Ignored);
    assert!(
        state.get(&art).trashed,
        "a restore below the advanced high-water must not apply"
    );
}

#[test]
fn test_encode_then_restore_reproduces_the_converged_visibility() {
    let mut state = VisibilityState::new();
    let trashed = artifact("root/pypi/flask/2.0.0");
    let revoked = artifact("root/oci/nginx");
    let visible = artifact("root/pypi/click/8.0.0");
    state.apply(&op(&trashed, Trash, 1, 5));
    state.apply(&op(&revoked, Revoke, 2, 9));
    state.apply(&op(&visible, Trash, 1, 3));
    state.apply(&op(&visible, Restore, 1, 4));

    let restored = VisibilityState::restore(&state.encode()).unwrap();

    assert!(restored.get(&trashed).trashed);
    assert!(restored.get(&revoked).revoked);
    assert!(restored.get(&visible).is_visible());
    assert_eq!(restored.retained_artifacts(), 3);
}

#[test]
fn test_restore_preserves_the_high_water_so_a_late_stale_op_is_ignored() {
    let mut state = VisibilityState::new();
    let art = artifact("a");
    state.apply(&op(&art, Trash, 1, 7));

    let mut restored = VisibilityState::restore(&state.encode()).unwrap();

    assert_eq!(restored.apply(&op(&art, Restore, 1, 6)), ApplyEffect::Ignored);
    assert!(
        restored.get(&art).trashed,
        "a stale restore below the restored high-water must not resurrect"
    );
}

#[test]
fn test_restore_rejects_an_unrecognized_schema() {
    let error = VisibilityState::restore(br#"{"schema":999,"artifacts":[]}"#).unwrap_err();
    assert!(matches!(
        error,
        SnapshotError::UnsupportedSchema {
            expected: VISIBILITY_APPLY_SCHEMA,
            found: 999
        }
    ));
}

#[test]
fn test_restore_rejects_malformed_bytes() {
    let error = VisibilityState::restore(b"not a snapshot").unwrap_err();
    assert!(matches!(error, SnapshotError::Malformed(_)));
}

#[test]
fn test_compact_releases_a_restored_artifact_the_frontier_covers() {
    let mut state = VisibilityState::new();
    let art = artifact("a");
    state.apply(&op(&art, Trash, 1, 5));
    state.apply(&op(&art, Restore, 1, 6));
    assert_eq!(state.retained_artifacts(), 1);

    state.compact(&frontier(1, 6));

    assert_eq!(state.retained_artifacts(), 0, "a covered, visible artifact is released");
    assert!(state.get(&art).is_visible());
}

#[test]
fn test_compact_keeps_a_trashed_tombstone_the_frontier_covers() {
    let mut state = VisibilityState::new();
    let art = artifact("a");
    state.apply(&op(&art, Trash, 1, 5));

    state.compact(&frontier(1, 5));

    assert_eq!(state.retained_artifacts(), 1);
    assert!(state.get(&art).trashed, "a trashed tombstone survives compaction");
}

#[test]
fn test_compact_keeps_a_revoked_tombstone_the_frontier_covers() {
    let mut state = VisibilityState::new();
    let art = artifact("a");
    state.apply(&op(&art, Revoke, 1, 5));

    state.compact(&frontier(1, 5));

    assert_eq!(state.retained_artifacts(), 1);
    assert!(state.get(&art).revoked, "a revoked tombstone survives compaction");
}

#[test]
fn test_compact_keeps_a_restored_artifact_above_the_frontier() {
    let mut state = VisibilityState::new();
    let art = artifact("a");
    state.apply(&op(&art, Trash, 1, 5));
    state.apply(&op(&art, Restore, 1, 10));

    state.compact(&frontier(1, 5));

    assert_eq!(
        state.retained_artifacts(),
        1,
        "an entry above the frontier is kept for a lagging replica"
    );
    assert!(state.get(&art).is_visible());
}

#[test]
fn test_compact_keeps_a_lifted_artifact_whose_revoke_dimension_is_above_the_frontier() {
    let mut state = VisibilityState::new();
    let art = artifact("a");
    state.apply(&op(&art, Trash, 1, 3));
    state.apply(&op(&art, Restore, 1, 4));
    state.apply(&op(&art, Revoke, 1, 8));
    state.apply(&op(&art, Lift, 1, 9));

    state.compact(&frontier(1, 4));

    assert_eq!(
        state.retained_artifacts(),
        1,
        "the covered trash dimension does not release an uncovered revoke dimension"
    );
    assert!(state.get(&art).is_visible());
}

#[test]
fn test_compact_keeps_an_artifact_in_an_unacknowledged_epoch() {
    let mut state = VisibilityState::new();
    let art = artifact("a");
    state.apply(&op(&art, Trash, 5, 1));
    state.apply(&op(&art, Restore, 5, 2));

    state.compact(&frontier(1, 100));

    assert_eq!(
        state.retained_artifacts(),
        1,
        "an unacknowledged epoch covers nothing, so the entry is kept"
    );
}

#[test]
fn test_frontier_keeps_the_highest_acknowledged_serial() {
    let mut state = VisibilityState::new();
    let art = artifact("a");
    state.apply(&op(&art, Trash, 1, 5));
    state.apply(&op(&art, Restore, 1, 6));

    let mut frontier = Frontier::default();
    frontier.acknowledge(1, 6);
    frontier.acknowledge(1, 4);
    state.compact(&frontier);

    assert_eq!(
        state.retained_artifacts(),
        0,
        "an out-of-order lower acknowledgement does not retract coverage"
    );
}

#[test]
fn test_compact_keeps_a_lift_in_a_later_epoch_until_the_revoke_epoch_drains() {
    let mut state = VisibilityState::new();
    let art = artifact("a");
    state.apply(&op(&art, Revoke, 1, 5));
    state.apply(&op(&art, Lift, 2, 1));

    // The lift's epoch is covered, but epoch 1 is not, so the entry that fences a late epoch-1 revoke
    // must survive: forgetting it would let the stale revoke resurrect the artifact.
    state.compact(&frontier(2, 1));

    assert_eq!(
        state.retained_artifacts(),
        1,
        "the fence for the undrained earlier epoch is kept"
    );
    assert_eq!(
        state.apply(&op(&art, Revoke, 1, 9)),
        ApplyEffect::Ignored,
        "a stale epoch-1 revoke stays fenced rather than resurrecting the lifted artifact",
    );
    assert!(state.get(&art).is_visible());
}

#[test]
fn test_compact_keeps_a_restore_in_a_later_epoch_until_the_trash_epoch_drains() {
    let mut state = VisibilityState::new();
    let art = artifact("a");
    state.apply(&op(&art, Trash, 1, 5));
    state.apply(&op(&art, Restore, 2, 1));

    // The symmetric trash dimension: a covered restore in a later epoch must not release the entry while
    // an earlier trash epoch can still deliver a stale trash.
    state.compact(&frontier(2, 1));

    assert_eq!(
        state.retained_artifacts(),
        1,
        "the fence for the undrained earlier epoch is kept"
    );
    assert_eq!(
        state.apply(&op(&art, Trash, 1, 9)),
        ApplyEffect::Ignored,
        "a stale epoch-1 trash stays fenced rather than resurrecting the restored artifact",
    );
    assert!(state.get(&art).is_visible());
}

#[test]
fn test_compact_releases_a_cross_epoch_entry_once_every_earlier_epoch_is_acknowledged() {
    let mut state = VisibilityState::new();
    let art = artifact("a");
    state.apply(&op(&art, Revoke, 1, 5));
    state.apply(&op(&art, Lift, 2, 1));

    // Epoch 1 is drained everywhere and epoch 2 covers the lift, so no earlier operation can still
    // arrive and the settled, visible entry is released.
    let mut frontier = Frontier::default();
    frontier.acknowledge(1, 5);
    frontier.acknowledge(2, 1);
    state.compact(&frontier);

    assert_eq!(
        state.retained_artifacts(),
        0,
        "a fully settled cross-epoch entry is released"
    );
    assert!(state.get(&art).is_visible());
}

#[test]
fn test_compact_keeps_a_cross_epoch_entry_with_an_unacknowledged_middle_epoch() {
    let mut state = VisibilityState::new();
    let art = artifact("a");
    state.apply(&op(&art, Revoke, 1, 5));
    state.apply(&op(&art, Lift, 3, 1));

    // Epoch 3 covers the lift and epoch 1 is acknowledged, but epoch 2 is not, so an epoch-2 operation
    // could still arrive: the gap below the high-water epoch keeps the entry.
    let mut frontier = Frontier::default();
    frontier.acknowledge(1, 5);
    frontier.acknowledge(3, 1);
    state.compact(&frontier);

    assert_eq!(
        state.retained_artifacts(),
        1,
        "a gap below the high-water epoch keeps the entry"
    );
    assert_eq!(
        state.apply(&op(&art, Revoke, 2, 4)),
        ApplyEffect::Ignored,
        "a stale epoch-2 revoke stays fenced",
    );
    assert!(state.get(&art).is_visible());
}
