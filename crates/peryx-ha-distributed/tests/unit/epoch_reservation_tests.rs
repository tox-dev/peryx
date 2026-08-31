//! Epoch zero means unassigned; the fence rejects it.

use crate::authority::{Admission, AuthorityFence, AuthorityKey, CommitOutcome};
use crate::envelope::AuthorityEpoch;
use crate::ownership::{
    AppliedMeta, AssignmentCause, DatacenterId, OwnershipCommand, OwnershipEffect, OwnershipState, Rejection,
};

const RESERVED: AuthorityEpoch = AuthorityEpoch(0);

const META: AppliedMeta = AppliedMeta { term: 1, index: 1 };

fn key(name: &str) -> AuthorityKey {
    AuthorityKey(name.to_owned())
}

fn assign(authority: &str, home: &str) -> OwnershipCommand {
    OwnershipCommand::AssignHome {
        authority: key(authority),
        home: DatacenterId(home.to_owned()),
        cause: AssignmentCause::FirstPublish,
    }
}

fn advance(authority: &str) -> OwnershipCommand {
    OwnershipCommand::AdvanceAuthorityEpoch {
        authority: key(authority),
        now_unix: 0,
    }
}

fn transfer(authority: &str, new_home: &str) -> OwnershipCommand {
    OwnershipCommand::RecordTransfer {
        authority: key(authority),
        new_home: DatacenterId(new_home.to_owned()),
        now_unix: 0,
    }
}

fn minted(effect: &OwnershipEffect) -> Option<AuthorityEpoch> {
    match effect {
        OwnershipEffect::Assigned { epoch, .. }
        | OwnershipEffect::EpochAdvanced { epoch }
        | OwnershipEffect::Transferred { epoch, .. } => Some(*epoch),
        OwnershipEffect::AlreadyAssigned { .. }
        | OwnershipEffect::Forgotten { .. }
        | OwnershipEffect::AlreadyForgotten
        | OwnershipEffect::WriteLeased { .. }
        | OwnershipEffect::WriteFinished
        | OwnershipEffect::SingletonAcquired { .. }
        | OwnershipEffect::SingletonHeld { .. }
        | OwnershipEffect::SingletonRenewed { .. }
        | OwnershipEffect::SingletonReleased
        | OwnershipEffect::Control(_)
        | OwnershipEffect::ControlSettled(_)
        | OwnershipEffect::ControlReleased
        | OwnershipEffect::Rejected(_) => None,
    }
}

#[test]
fn test_the_producer_reserves_zero_for_an_unassigned_authority() {
    let state = OwnershipState::new();

    assert_eq!(state.epoch(&key("proj")), RESERVED);
    assert_eq!(state.home(&key("proj")), None);
}

#[test]
fn test_a_rejected_command_mints_no_epoch() {
    assert_eq!(minted(&OwnershipEffect::Rejected(Rejection::NotAssigned)), None);
}

#[test]
fn test_the_producer_mints_the_first_real_epoch_one_past_the_reserved_sentinel() {
    let mut state = OwnershipState::new();

    let effect = state.apply(&assign("proj", "east"), META);

    assert_eq!(minted(&effect), Some(AuthorityEpoch(RESERVED.0 + 1)));
    assert_eq!(state.epoch(&key("proj")), AuthorityEpoch(1));
    assert_ne!(state.epoch(&key("proj")), RESERVED);
}

#[test]
fn test_the_producer_never_mints_the_reserved_epoch_across_a_command_sequence() {
    let mut state = OwnershipState::new();
    let commands = [
        assign("proj", "east"),
        advance("proj"),
        transfer("proj", "west"),
        advance("proj"),
        transfer("proj", "north"),
    ];

    for command in &commands {
        let effect = state.apply(command, META);
        assert_ne!(
            minted(&effect),
            Some(RESERVED),
            "a minted epoch is never the reserved sentinel"
        );
        assert_ne!(
            state.epoch(&key("proj")),
            RESERVED,
            "a homed authority never reads as unassigned"
        );
    }
}

#[test]
fn test_the_fence_admits_every_epoch_the_producer_mints_and_fences_the_reserved_one() {
    let mut state = OwnershipState::new();
    let mut fence = AuthorityFence::new();
    let authority = key("proj");

    assert_eq!(fence.commit(&authority, RESERVED), CommitOutcome::Ignored);
    assert!(matches!(
        fence.admit(&authority, AuthorityEpoch(1)),
        Admission::Fenced { .. }
    ));

    for command in [assign("proj", "east"), advance("proj"), transfer("proj", "west")] {
        let epoch = minted(&state.apply(&command, META)).expect("each command mints an epoch");
        assert_eq!(fence.commit(&authority, epoch), CommitOutcome::Committed);
        assert_eq!(fence.admit(&authority, epoch), Admission::Admit);
    }
}
