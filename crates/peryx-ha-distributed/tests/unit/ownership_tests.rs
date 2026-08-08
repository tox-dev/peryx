use crate::authority::{Admission, AuthorityFence, AuthorityKey};
use crate::envelope::AuthorityEpoch;
use crate::ownership::{
    AppliedMeta, Assignment, AssignmentCause, DatacenterId, OwnershipCommand, OwnershipEffect, OwnershipError,
    OwnershipState, Rejection, TransferRecord,
};

fn key(name: &str) -> AuthorityKey {
    AuthorityKey(name.to_owned())
}

fn dc(name: &str) -> DatacenterId {
    DatacenterId(name.to_owned())
}

/// A stand-in committed log position for a command whose exact position a test does not assert.
const META: AppliedMeta = AppliedMeta { term: 1, index: 1 };

fn assign(authority: &str, home: &str) -> OwnershipCommand {
    OwnershipCommand::AssignHome {
        authority: key(authority),
        home: dc(home),
        cause: AssignmentCause::FirstPublish,
    }
}

fn advance(authority: &str) -> OwnershipCommand {
    OwnershipCommand::AdvanceAuthorityEpoch {
        authority: key(authority),
    }
}

fn transfer(authority: &str, new_home: &str) -> OwnershipCommand {
    OwnershipCommand::RecordTransfer {
        authority: key(authority),
        new_home: dc(new_home),
    }
}

#[test]
fn test_assign_home_mints_epoch_one_and_records_the_home() {
    let mut state = OwnershipState::new();

    let effect = state.apply(&assign("proj", "east"), META);

    assert_eq!(
        effect,
        OwnershipEffect::Assigned {
            epoch: AuthorityEpoch(1)
        }
    );
    assert_eq!(state.epoch(&key("proj")), AuthorityEpoch(1));
    assert_eq!(state.home(&key("proj")), Some(&dc("east")));
}

#[test]
fn test_assign_home_records_the_assignment_audit_from_the_committed_position() {
    let mut state = OwnershipState::new();

    state.apply(&assign("proj", "east"), AppliedMeta { term: 4, index: 27 });

    assert_eq!(
        state.assignment(&key("proj")),
        Some(&Assignment {
            cause: AssignmentCause::FirstPublish,
            term: 4,
            index: 27,
            epoch: AuthorityEpoch(1),
        })
    );
}

#[test]
fn test_an_unassigned_authority_has_no_assignment_audit() {
    let state = OwnershipState::new();

    assert_eq!(state.assignment(&key("proj")), None);
}

#[test]
fn test_a_rejected_reassignment_keeps_the_first_assignment_audit() {
    let mut state = OwnershipState::new();
    state.apply(&assign("proj", "east"), AppliedMeta { term: 2, index: 9 });

    state.apply(&assign("proj", "west"), AppliedMeta { term: 5, index: 40 });

    // The second command committed at a later position but was rejected, so the audit still names the
    // winning first assignment, not the loser's position.
    assert_eq!(
        state.assignment(&key("proj")),
        Some(&Assignment {
            cause: AssignmentCause::FirstPublish,
            term: 2,
            index: 9,
            epoch: AuthorityEpoch(1),
        })
    );
}

#[test]
fn test_an_advance_leaves_the_initial_assignment_audit_unchanged() {
    let mut state = OwnershipState::new();
    state.apply(&assign("proj", "east"), AppliedMeta { term: 3, index: 11 });

    state.apply(&advance("proj"), AppliedMeta { term: 3, index: 12 });

    // The audit is the initial home assignment; advancing the epoch does not rewrite it, so its epoch
    // stays one even as the authority's current epoch moves on.
    assert_eq!(state.epoch(&key("proj")), AuthorityEpoch(2));
    assert_eq!(
        state.assignment(&key("proj")),
        Some(&Assignment {
            cause: AssignmentCause::FirstPublish,
            term: 3,
            index: 11,
            epoch: AuthorityEpoch(1),
        })
    );
}

#[test]
fn test_an_unassigned_authority_reads_as_the_zero_sentinel() {
    let state = OwnershipState::new();

    assert_eq!(state.epoch(&key("proj")), AuthorityEpoch(0));
    assert_eq!(state.home(&key("proj")), None);
    assert!(state.transfers(&key("proj")).is_empty());
}

#[test]
fn test_assigning_an_already_homed_authority_is_rejected_and_leaves_it_unchanged() {
    let mut state = OwnershipState::new();
    state.apply(&assign("proj", "east"), META);

    let effect = state.apply(&assign("proj", "west"), META);

    assert_eq!(effect, OwnershipEffect::Rejected(Rejection::AlreadyAssigned));
    assert_eq!(state.epoch(&key("proj")), AuthorityEpoch(1));
    assert_eq!(state.home(&key("proj")), Some(&dc("east")));
}

#[test]
fn test_advancing_the_epoch_increments_it_and_keeps_the_home() {
    let mut state = OwnershipState::new();
    state.apply(&assign("proj", "east"), META);

    let effect = state.apply(&advance("proj"), META);

    assert_eq!(
        effect,
        OwnershipEffect::EpochAdvanced {
            epoch: AuthorityEpoch(2)
        }
    );
    assert_eq!(state.epoch(&key("proj")), AuthorityEpoch(2));
    assert_eq!(state.home(&key("proj")), Some(&dc("east")));
    assert!(state.transfers(&key("proj")).is_empty());
}

#[test]
fn test_advancing_is_monotonic_across_repeated_commands() {
    let mut state = OwnershipState::new();
    state.apply(&assign("proj", "east"), META);

    state.apply(&advance("proj"), META);
    state.apply(&advance("proj"), META);

    assert_eq!(state.epoch(&key("proj")), AuthorityEpoch(3));
}

#[test]
fn test_advancing_an_unassigned_authority_is_rejected() {
    let mut state = OwnershipState::new();

    let effect = state.apply(&advance("proj"), META);

    assert_eq!(effect, OwnershipEffect::Rejected(Rejection::NotAssigned));
    assert_eq!(state.epoch(&key("proj")), AuthorityEpoch(0));
}

#[test]
fn test_transfer_moves_the_home_mints_the_next_epoch_and_records_the_move() {
    let mut state = OwnershipState::new();
    state.apply(&assign("proj", "east"), META);

    let effect = state.apply(&transfer("proj", "west"), META);

    assert_eq!(
        effect,
        OwnershipEffect::Transferred {
            from: dc("east"),
            to: dc("west"),
            epoch: AuthorityEpoch(2),
        }
    );
    assert_eq!(state.epoch(&key("proj")), AuthorityEpoch(2));
    assert_eq!(state.home(&key("proj")), Some(&dc("west")));
    assert_eq!(
        state.transfers(&key("proj")),
        &[TransferRecord {
            from: dc("east"),
            to: dc("west"),
            epoch: AuthorityEpoch(2),
        }]
    );
}

#[test]
fn test_successive_transfers_keep_the_full_trail_in_order() {
    let mut state = OwnershipState::new();
    state.apply(&assign("proj", "east"), META);

    state.apply(&transfer("proj", "west"), META);
    state.apply(&transfer("proj", "north"), META);

    assert_eq!(
        state.transfers(&key("proj")),
        &[
            TransferRecord {
                from: dc("east"),
                to: dc("west"),
                epoch: AuthorityEpoch(2),
            },
            TransferRecord {
                from: dc("west"),
                to: dc("north"),
                epoch: AuthorityEpoch(3),
            },
        ]
    );
    assert_eq!(state.home(&key("proj")), Some(&dc("north")));
}

#[test]
fn test_transferring_an_unassigned_authority_is_rejected() {
    let mut state = OwnershipState::new();

    let effect = state.apply(&transfer("proj", "west"), META);

    assert_eq!(effect, OwnershipEffect::Rejected(Rejection::NotAssigned));
    assert_eq!(state.home(&key("proj")), None);
}

#[test]
fn test_transferring_to_the_current_home_is_rejected_and_mints_no_epoch() {
    let mut state = OwnershipState::new();
    state.apply(&assign("proj", "east"), META);

    let effect = state.apply(&transfer("proj", "east"), META);

    assert_eq!(effect, OwnershipEffect::Rejected(Rejection::SameHome));
    assert_eq!(state.epoch(&key("proj")), AuthorityEpoch(1));
    assert!(state.transfers(&key("proj")).is_empty());
}

#[test]
fn test_authorities_move_independently() {
    let mut state = OwnershipState::new();
    state.apply(&assign("alpha", "east"), META);
    state.apply(&assign("beta", "west"), META);

    state.apply(&advance("alpha"), META);

    assert_eq!(state.epoch(&key("alpha")), AuthorityEpoch(2));
    assert_eq!(state.epoch(&key("beta")), AuthorityEpoch(1));
    assert_eq!(state.home(&key("beta")), Some(&dc("west")));
}

#[test]
fn test_a_minted_epoch_admits_at_the_fence_and_fences_the_prior_one() {
    // The state machine is the producer the fence consumes: the epoch it mints, committed, is the one
    // the fence admits, and the epoch it superseded is fenced.
    let mut state = OwnershipState::new();
    let mut fence = AuthorityFence::new();
    let authority = key("proj");

    let OwnershipEffect::Assigned { epoch: first } = state.apply(&assign("proj", "east"), META) else {
        panic!("assign should mint the first epoch");
    };
    fence.commit(&authority, first);

    let OwnershipEffect::Transferred { epoch: second, .. } = state.apply(&transfer("proj", "west"), META) else {
        panic!("transfer should mint the next epoch");
    };
    fence.commit(&authority, second);

    assert_eq!(fence.admit(&authority, second), Admission::Admit);
    assert_eq!(
        fence.admit(&authority, first),
        Admission::Fenced {
            committed: second,
            presented: first,
        }
    );
}

#[test]
fn test_snapshot_restore_round_trips_the_full_state() {
    let mut state = OwnershipState::new();
    state.apply(&assign("alpha", "east"), AppliedMeta { term: 1, index: 3 });
    state.apply(&transfer("alpha", "west"), META);
    state.apply(&assign("beta", "north"), AppliedMeta { term: 1, index: 5 });
    state.apply(&advance("beta"), META);

    let restored = OwnershipState::restore(&state.snapshot()).unwrap();

    assert_eq!(restored, state);
    assert_eq!(restored.epoch(&key("alpha")), AuthorityEpoch(2));
    assert_eq!(restored.home(&key("alpha")), Some(&dc("west")));
    assert_eq!(
        restored.transfers(&key("alpha")),
        &[TransferRecord {
            from: dc("east"),
            to: dc("west"),
            epoch: AuthorityEpoch(2),
        }]
    );
    // The assignment audit survives the snapshot round-trip, so a replay reconstructs where each home
    // was first assigned.
    assert_eq!(
        restored.assignment(&key("alpha")),
        Some(&Assignment {
            cause: AssignmentCause::FirstPublish,
            term: 1,
            index: 3,
            epoch: AuthorityEpoch(1),
        })
    );
    assert_eq!(restored.epoch(&key("beta")), AuthorityEpoch(2));
}

#[test]
fn test_an_empty_state_round_trips() {
    let restored = OwnershipState::restore(&OwnershipState::new().snapshot()).unwrap();

    assert_eq!(restored, OwnershipState::new());
}

#[test]
fn test_restore_rejects_malformed_bytes() {
    let error = OwnershipState::restore(b"not a snapshot").unwrap_err();

    assert!(matches!(error, OwnershipError::Malformed(_)));
}

#[test]
fn test_restore_rejects_a_record_at_the_reserved_zero_epoch() {
    let snapshot = br#"{"proj":{"home":"east","epoch":0,"assignment":{"cause":"first-publish","term":1,"index":1,"epoch":1},"transfers":[]}}"#;

    let error = OwnershipState::restore(snapshot).unwrap_err();

    assert!(matches!(error, OwnershipError::ZeroEpoch { authority } if authority == "proj"));
}
