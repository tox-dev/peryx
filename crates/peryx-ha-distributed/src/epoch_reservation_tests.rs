//! The authority-epoch producer reserves `0` and mints one-based epochs.
//!
//! The [`AuthorityFence`] fails closed on the [`AuthorityEpoch(0)`](crate::AuthorityEpoch) sentinel: it
//! admits work only under a nonzero committed epoch, so an unassigned authority fences everything. That
//! guard is correct only while the epoch producer — the [`OwnershipState`] the Raft ownership machine
//! drives — reserves `0` for "unassigned" and never hands a real authority epoch `0`. If it ever homed
//! an authority at epoch `0`, that authority could never commit at the fence (`commit(0)` is
//! [`Ignored`](CommitOutcome::Ignored)) and every operation it presented would be
//! [`Fenced`](Admission::Fenced) forever. These tests confirm the contract at the producer layer.

use crate::authority::{Admission, AuthorityFence, AuthorityKey, CommitOutcome};
use crate::envelope::AuthorityEpoch;
use crate::ownership::{AppliedMeta, AssignmentCause, DatacenterId, OwnershipCommand, OwnershipEffect, OwnershipState};

/// The reserved unassigned sentinel the fence fails closed on.
const RESERVED: AuthorityEpoch = AuthorityEpoch(0);

/// A stand-in committed log position for a command whose exact position these tests do not assert.
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
    }
}

fn transfer(authority: &str, new_home: &str) -> OwnershipCommand {
    OwnershipCommand::RecordTransfer {
        authority: key(authority),
        new_home: DatacenterId(new_home.to_owned()),
    }
}

/// The epoch an effect minted, or `None` for a rejection that minted nothing.
fn minted(effect: &OwnershipEffect) -> Option<AuthorityEpoch> {
    match effect {
        OwnershipEffect::Assigned { epoch }
        | OwnershipEffect::EpochAdvanced { epoch }
        | OwnershipEffect::Transferred { epoch, .. } => Some(*epoch),
        OwnershipEffect::Rejected(_) => None,
    }
}

#[test]
fn test_the_producer_reserves_zero_for_an_unassigned_authority() {
    let state = OwnershipState::new();

    // The producer reads an authority no command has homed as the reserved sentinel, the same value the
    // fence fails closed on, so the two agree on what "unassigned" is.
    assert_eq!(state.epoch(&key("proj")), RESERVED);
    assert_eq!(state.home(&key("proj")), None);
}

#[test]
fn test_the_producer_mints_the_first_real_epoch_one_past_the_reserved_sentinel() {
    let mut state = OwnershipState::new();

    let effect = state.apply(&assign("proj", "east"), META);

    // A first publish mints epoch one, not zero: the reserved sentinel is skipped, so a real authority
    // always carries a nonzero epoch.
    assert_eq!(minted(&effect), Some(AuthorityEpoch(RESERVED.0 + 1)));
    assert_eq!(state.epoch(&key("proj")), AuthorityEpoch(1));
    assert_ne!(state.epoch(&key("proj")), RESERVED);
}

#[test]
fn test_the_producer_never_mints_the_reserved_epoch_across_a_command_sequence() {
    let mut state = OwnershipState::new();
    // A homed authority put through every mutation the producer offers: advances and transfers, which are
    // the only commands that mint a new epoch after the first assignment.
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

    // The reserved sentinel never commits, so an unassigned authority fences all work — the fence's
    // fail-closed guard, and the reason the producer must never hand it a real authority at epoch zero.
    assert_eq!(fence.commit(&authority, RESERVED), CommitOutcome::Ignored);
    assert!(matches!(
        fence.admit(&authority, AuthorityEpoch(1)),
        Admission::Fenced { .. }
    ));

    // Every epoch the producer mints is nonzero, so the fence commits it and admits work under it.
    for command in [assign("proj", "east"), advance("proj"), transfer("proj", "west")] {
        let epoch = minted(&state.apply(&command, META)).expect("each command mints an epoch");
        assert_eq!(fence.commit(&authority, epoch), CommitOutcome::Committed);
        assert_eq!(fence.admit(&authority, epoch), Admission::Admit);
    }
}
