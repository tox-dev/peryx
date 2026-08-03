use crate::visibility::VisibilityAction::{Lift, Restore, Revoke, Trash};
use crate::visibility::{
    ApplyEffect, ArtifactId, OpOrder, Visibility, VisibilityAction, VisibilityOp, VisibilityState,
};

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
