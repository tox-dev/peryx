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
        now_unix: 0,
    }
}

fn transfer(authority: &str, new_home: &str) -> OwnershipCommand {
    transfer_at(authority, new_home, 0)
}

fn transfer_at(authority: &str, new_home: &str, now_unix: i64) -> OwnershipCommand {
    OwnershipCommand::RecordTransfer {
        authority: key(authority),
        new_home: dc(new_home),
        now_unix,
    }
}

fn begin_write(authority: &str, epoch: u64, id: &str, issued_at_unix: i64) -> OwnershipCommand {
    OwnershipCommand::BeginEpochWrite {
        authority: key(authority),
        epoch: AuthorityEpoch(epoch),
        id: id.to_owned(),
        issued_at_unix,
        expires_at_unix: issued_at_unix + peryx_ha::AUTHORITY_WRITE_LEASE_SECS,
    }
}

fn finish_write(authority: &str, epoch: u64, id: &str) -> OwnershipCommand {
    OwnershipCommand::FinishEpochWrite {
        authority: key(authority),
        epoch: AuthorityEpoch(epoch),
        id: id.to_owned(),
    }
}

#[test]
fn test_assign_home_mints_epoch_one_and_records_the_home() {
    let mut state = OwnershipState::new();

    let effect = state.apply(&assign("proj", "east"), META);

    assert_eq!(
        effect,
        OwnershipEffect::Assigned {
            home: dc("east"),
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
fn test_a_reassignment_keeps_the_first_assignment_audit() {
    let mut state = OwnershipState::new();
    state.apply(&assign("proj", "east"), AppliedMeta { term: 2, index: 9 });

    state.apply(&assign("proj", "west"), AppliedMeta { term: 5, index: 40 });

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
fn test_assigning_an_already_homed_authority_returns_its_claim_and_leaves_it_unchanged() {
    let mut state = OwnershipState::new();
    state.apply(&assign("proj", "east"), META);

    let effect = state.apply(&assign("proj", "west"), META);

    assert_eq!(
        effect,
        OwnershipEffect::AlreadyAssigned {
            home: DatacenterId("east".to_owned()),
            epoch: AuthorityEpoch(1),
        }
    );
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
fn test_write_lease_requires_the_committed_epoch() {
    let mut state = OwnershipState::new();
    state.apply(&assign("proj", "east"), META);

    assert_eq!(
        state.apply(&begin_write("proj", 2, "write-1", 100), META),
        OwnershipEffect::Rejected(Rejection::EpochMismatch)
    );
}

#[test]
fn test_write_lease_rejects_a_caller_supplied_long_expiry() {
    let mut state = OwnershipState::new();
    state.apply(&assign("proj", "east"), META);

    assert_eq!(
        state.apply(
            &OwnershipCommand::BeginEpochWrite {
                authority: key("proj"),
                epoch: AuthorityEpoch(1),
                id: "write-1".to_owned(),
                issued_at_unix: 100,
                expires_at_unix: 100 + peryx_ha::AUTHORITY_WRITE_LEASE_SECS + 1,
            },
            META,
        ),
        OwnershipEffect::Rejected(Rejection::InvalidLease)
    );
}

#[test]
fn test_write_lease_rejects_an_empty_window() {
    let mut state = OwnershipState::new();
    state.apply(&assign("proj", "east"), META);

    assert_eq!(
        state.apply(
            &OwnershipCommand::BeginEpochWrite {
                authority: key("proj"),
                epoch: AuthorityEpoch(1),
                id: "write-1".to_owned(),
                issued_at_unix: 100,
                expires_at_unix: 100,
            },
            META,
        ),
        OwnershipEffect::Rejected(Rejection::InvalidLease)
    );
}

#[test]
fn test_write_commands_reject_an_unassigned_authority() {
    let mut state = OwnershipState::new();

    for command in [
        begin_write("proj", 1, "write-1", 100),
        finish_write("proj", 1, "write-1"),
    ] {
        assert_eq!(
            state.apply(&command, META),
            OwnershipEffect::Rejected(Rejection::NotAssigned)
        );
    }
}

#[test]
fn test_finish_rejects_the_wrong_epoch_for_a_live_id() {
    let mut state = OwnershipState::new();
    state.apply(&assign("proj", "east"), META);
    state.apply(&begin_write("proj", 1, "write-1", 100), META);

    assert_eq!(
        state.apply(&finish_write("proj", 2, "write-1"), META),
        OwnershipEffect::Rejected(Rejection::EpochMismatch)
    );
    assert_eq!(
        state.apply(&transfer("proj", "west"), META),
        OwnershipEffect::Rejected(Rejection::WritesInFlight)
    );
}

#[test]
fn test_finishing_an_absent_id_is_idempotent() {
    let mut state = OwnershipState::new();
    state.apply(&assign("proj", "east"), META);

    assert_eq!(
        state.apply(&finish_write("proj", 1, "write-1"), META),
        OwnershipEffect::WriteFinished
    );
}

#[test]
fn test_active_write_lease_blocks_transfer_until_finish() {
    let mut state = OwnershipState::new();
    state.apply(&assign("proj", "east"), META);
    state.apply(&begin_write("proj", 1, "write-1", 100), META);

    assert_eq!(
        state.apply(&transfer("proj", "west"), META),
        OwnershipEffect::Rejected(Rejection::WritesInFlight)
    );
    assert_eq!(
        state.apply(&finish_write("proj", 1, "write-1"), META),
        OwnershipEffect::WriteFinished
    );
    assert!(matches!(
        state.apply(&transfer("proj", "west"), META),
        OwnershipEffect::Transferred {
            epoch: AuthorityEpoch(2),
            ..
        }
    ));
}

#[test]
fn test_active_write_lease_blocks_epoch_advance() {
    let mut state = OwnershipState::new();
    state.apply(&assign("proj", "east"), META);
    state.apply(&begin_write("proj", 1, "write-1", 100), META);

    assert_eq!(
        state.apply(&advance("proj"), META),
        OwnershipEffect::Rejected(Rejection::WritesInFlight)
    );
    assert_eq!(state.epoch(&key("proj")), AuthorityEpoch(1));
}

#[test]
fn test_a_new_write_command_prunes_skew_safe_expired_leases() {
    let mut state = OwnershipState::new();
    state.apply(&assign("proj", "east"), META);
    state.apply(&begin_write("proj", 1, "write-1", 100), META);

    state.apply(
        &begin_write(
            "proj",
            1,
            "write-2",
            100 + peryx_ha::AUTHORITY_WRITE_LEASE_SECS + peryx_ha::AUTHORITY_CLOCK_SKEW_SECS,
        ),
        META,
    );
    state.apply(&finish_write("proj", 1, "write-2"), META);

    assert!(matches!(
        state.apply(&transfer("proj", "west"), META),
        OwnershipEffect::Transferred { .. }
    ));
}

#[test]
fn test_transfer_waits_through_the_clock_skew_guard() {
    let mut state = OwnershipState::new();
    state.apply(&assign("proj", "east"), META);
    state.apply(&begin_write("proj", 1, "write-1", 100), META);

    assert_eq!(
        state.apply(
            &transfer_at(
                "proj",
                "west",
                100 + peryx_ha::AUTHORITY_WRITE_LEASE_SECS + peryx_ha::AUTHORITY_CLOCK_SKEW_SECS - 1,
            ),
            META,
        ),
        OwnershipEffect::Rejected(Rejection::WritesInFlight)
    );
}

#[test]
fn test_expired_write_lease_releases_transfer_and_new_epoch_admission() {
    let mut state = OwnershipState::new();
    state.apply(&assign("proj", "east"), META);
    state.apply(&begin_write("proj", 1, "write-1", 100), META);

    assert!(matches!(
        state.apply(
            &transfer_at(
                "proj",
                "west",
                100 + peryx_ha::AUTHORITY_WRITE_LEASE_SECS + peryx_ha::AUTHORITY_CLOCK_SKEW_SECS,
            ),
            META,
        ),
        OwnershipEffect::Transferred {
            epoch: AuthorityEpoch(2),
            ..
        }
    ));
    assert!(matches!(
        state.apply(&begin_write("proj", 2, "write-2", 200), META),
        OwnershipEffect::WriteLeased {
            epoch: AuthorityEpoch(2),
            ..
        }
    ));
}

#[test]
fn test_transfer_waits_for_all_live_write_ids() {
    let mut state = OwnershipState::new();
    state.apply(&assign("proj", "east"), META);
    state.apply(&begin_write("proj", 1, "write-1", 100), META);
    state.apply(&begin_write("proj", 1, "write-2", 100), META);

    state.apply(&finish_write("proj", 1, "write-1"), META);
    assert_eq!(
        state.apply(&transfer("proj", "west"), META),
        OwnershipEffect::Rejected(Rejection::WritesInFlight)
    );
    state.apply(&finish_write("proj", 1, "write-2"), META);
    assert!(matches!(
        state.apply(&transfer("proj", "west"), META),
        OwnershipEffect::Transferred { .. }
    ));
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
    let mut state = OwnershipState::new();
    let mut fence = AuthorityFence::new();
    let authority = key("proj");

    let first = AuthorityEpoch(1);
    assert_eq!(
        state.apply(&assign("proj", "east"), META),
        OwnershipEffect::Assigned {
            home: dc("east"),
            epoch: first,
        }
    );
    fence.commit(&authority, first);

    let second = AuthorityEpoch(2);
    assert_eq!(
        state.apply(&transfer("proj", "west"), META),
        OwnershipEffect::Transferred {
            from: dc("east"),
            to: dc("west"),
            epoch: second,
        }
    );
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
    state.apply(&begin_write("beta", 2, "write-1", 100), META);

    let mut restored = OwnershipState::restore(&state.snapshot()).unwrap();

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
    assert_eq!(
        restored.apply(&transfer("beta", "south"), META),
        OwnershipEffect::Rejected(Rejection::WritesInFlight)
    );
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
