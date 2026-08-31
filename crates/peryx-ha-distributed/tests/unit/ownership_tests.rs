use crate::authority::{Admission, AuthorityFence, AuthorityKey};
use crate::envelope::AuthorityEpoch;
use crate::ownership::{
    AppliedMeta, Assignment, AssignmentCause, DatacenterId, OwnershipCommand, OwnershipEffect, OwnershipError,
    OwnershipState, Rejection,
};
use rstest::rstest;

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

fn forget(authority: &str) -> OwnershipCommand {
    forget_at(authority, 0)
}

fn forget_at(authority: &str, now_unix: i64) -> OwnershipCommand {
    OwnershipCommand::ForgetAuthority {
        authority: key(authority),
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

const JOB: &str = "reclamation";

fn acquire(holder: &str, now_unix: i64) -> OwnershipCommand {
    OwnershipCommand::AcquireSingletonLease {
        job: JOB.to_owned(),
        holder: holder.to_owned(),
        now_unix,
        expires_at_unix: now_unix + peryx_ha::SINGLETON_LEASE_SECS,
    }
}

fn renew(holder: &str, term: u64, generation: u64, now_unix: i64) -> OwnershipCommand {
    OwnershipCommand::RenewSingletonLease {
        job: JOB.to_owned(),
        holder: holder.to_owned(),
        term,
        generation,
        now_unix,
        expires_at_unix: now_unix + peryx_ha::SINGLETON_LEASE_SECS,
    }
}

fn release(holder: &str, term: u64, generation: u64, now_unix: i64) -> OwnershipCommand {
    OwnershipCommand::ReleaseSingletonLease {
        job: JOB.to_owned(),
        holder: holder.to_owned(),
        term,
        generation,
        now_unix,
    }
}

fn granted(holder: &str, term: u64, generation: u64, now_unix: i64) -> OwnershipEffect {
    OwnershipEffect::SingletonAcquired {
        holder: holder.to_owned(),
        term,
        generation,
        expires_at_unix: now_unix + peryx_ha::SINGLETON_LEASE_SECS,
    }
}

/// The point at which a grant taken at `now_unix` stops excluding another holder.
fn lapse(now_unix: i64) -> i64 {
    now_unix + peryx_ha::SINGLETON_LEASE_SECS + peryx_ha::AUTHORITY_CLOCK_SKEW_SECS
}

fn held_singleton(state: &mut OwnershipState, holder: &str, now_unix: i64) -> OwnershipEffect {
    state.apply(&acquire(holder, now_unix), META)
}

#[test]
fn test_a_first_singleton_claim_is_granted_at_generation_one() {
    let mut state = OwnershipState::new();

    assert_eq!(held_singleton(&mut state, "node-a", 100), granted("node-a", 1, 1, 100));
}

#[test]
fn test_the_committed_entry_supplies_the_grant_term() {
    let mut state = OwnershipState::new();

    let effect = state.apply(&acquire("node-a", 100), AppliedMeta { term: 9, index: 2 });

    assert_eq!(effect, granted("node-a", 9, 1, 100));
}

#[test]
fn test_a_second_holder_cannot_take_an_unlapsed_grant() {
    let mut state = OwnershipState::new();
    held_singleton(&mut state, "node-a", 100);

    let effect = state.apply(&acquire("node-b", 100 + peryx_ha::SINGLETON_LEASE_SECS), META);

    assert_eq!(
        effect,
        OwnershipEffect::SingletonHeld {
            holder: "node-a".to_owned(),
        }
    );
}

#[test]
fn test_a_later_leader_term_does_not_steal_an_unlapsed_grant() {
    let mut state = OwnershipState::new();
    state.apply(&acquire("node-a", 100), AppliedMeta { term: 1, index: 1 });

    let effect = state.apply(&acquire("node-b", 110), AppliedMeta { term: 2, index: 9 });

    assert_eq!(
        effect,
        OwnershipEffect::SingletonHeld {
            holder: "node-a".to_owned(),
        }
    );
}

#[test]
fn test_a_lapsed_grant_lets_the_next_holder_in_at_a_higher_generation() {
    let mut state = OwnershipState::new();
    held_singleton(&mut state, "node-a", 100);

    let effect = state.apply(&acquire("node-b", lapse(100)), META);

    assert_eq!(effect, granted("node-b", 1, 2, lapse(100)));
}

#[test]
fn test_the_owner_renews_its_own_grant() {
    let mut state = OwnershipState::new();
    held_singleton(&mut state, "node-a", 100);

    let effect = state.apply(&renew("node-a", 1, 1, 110), META);

    assert_eq!(
        effect,
        OwnershipEffect::SingletonRenewed {
            expires_at_unix: 110 + peryx_ha::SINGLETON_LEASE_SECS,
        }
    );
}

#[test]
fn test_renewal_extends_the_grant_past_the_original_deadline() {
    let mut state = OwnershipState::new();
    held_singleton(&mut state, "node-a", 100);
    state.apply(&renew("node-a", 1, 1, 110), META);

    let effect = state.apply(&acquire("node-b", lapse(100)), META);

    assert_eq!(
        effect,
        OwnershipEffect::SingletonHeld {
            holder: "node-a".to_owned(),
        }
    );
}

#[rstest]
#[case::another_holder(renew("node-b", 1, 1, 110))]
#[case::another_term(renew("node-a", 2, 1, 110))]
#[case::another_generation(renew("node-a", 1, 2, 110))]
#[case::after_the_grant_lapsed(renew("node-a", 1, 1, lapse(100)))]
fn test_a_renewal_that_no_longer_owns_the_grant_is_refused(#[case] command: OwnershipCommand) {
    let mut state = OwnershipState::new();
    held_singleton(&mut state, "node-a", 100);

    assert_eq!(
        state.apply(&command, META),
        OwnershipEffect::Rejected(Rejection::SingletonLost)
    );
}

#[test]
fn test_a_refused_renewal_leaves_the_grant_with_its_owner() {
    let mut state = OwnershipState::new();
    held_singleton(&mut state, "node-a", 100);

    state.apply(&renew("node-b", 1, 1, 110), META);

    assert_eq!(
        state.apply(&acquire("node-b", 110), META),
        OwnershipEffect::SingletonHeld {
            holder: "node-a".to_owned(),
        }
    );
}

#[rstest]
#[case::acquire(OwnershipCommand::AcquireSingletonLease {
    job: JOB.to_owned(),
    holder: "node-a".to_owned(),
    now_unix: 100,
    expires_at_unix: 100 + peryx_ha::SINGLETON_LEASE_SECS + 1,
})]
#[case::renew(OwnershipCommand::RenewSingletonLease {
    job: JOB.to_owned(),
    holder: "node-a".to_owned(),
    term: 1,
    generation: 1,
    now_unix: 100,
    expires_at_unix: 100,
})]
fn test_a_grant_longer_than_the_lease_policy_is_refused(#[case] command: OwnershipCommand) {
    let mut state = OwnershipState::new();

    assert_eq!(
        state.apply(&command, META),
        OwnershipEffect::Rejected(Rejection::InvalidLease)
    );
}

#[test]
fn test_the_owner_releases_its_grant() {
    let mut state = OwnershipState::new();
    held_singleton(&mut state, "node-a", 100);

    assert_eq!(
        state.apply(&release("node-a", 1, 1, 110), META),
        OwnershipEffect::SingletonReleased
    );
}

#[rstest]
#[case::another_holder(release("node-b", 1, 1, 110))]
#[case::another_term(release("node-a", 2, 1, 110))]
#[case::another_generation(release("node-a", 1, 2, 110))]
#[case::after_the_grant_lapsed(release("node-a", 1, 1, lapse(100)))]
fn test_a_release_that_no_longer_owns_the_grant_reports_the_loss(#[case] command: OwnershipCommand) {
    let mut state = OwnershipState::new();
    held_singleton(&mut state, "node-a", 100);

    assert_eq!(
        state.apply(&command, META),
        OwnershipEffect::Rejected(Rejection::SingletonLost)
    );
}

#[rstest]
#[case::renew(renew("node-a", 1, 1, 110))]
#[case::release(release("node-a", 1, 1, 110))]
fn test_a_request_against_a_job_that_was_never_granted_reports_the_loss(#[case] command: OwnershipCommand) {
    let mut state = OwnershipState::new();

    assert_eq!(
        state.apply(&command, META),
        OwnershipEffect::Rejected(Rejection::SingletonLost)
    );
}

#[test]
fn test_reacquisition_in_one_term_mints_a_distinct_generation() {
    let mut state = OwnershipState::new();
    held_singleton(&mut state, "node-a", 100);
    state.apply(&release("node-a", 1, 1, 110), META);

    assert_eq!(held_singleton(&mut state, "node-b", 110), granted("node-b", 1, 2, 110));
}

#[rstest]
#[case::renew(renew("node-a", 1, 1, 120))]
#[case::release(release("node-a", 1, 1, 120))]
fn test_a_delayed_request_from_the_previous_generation_is_refused(#[case] command: OwnershipCommand) {
    let mut state = OwnershipState::new();
    held_singleton(&mut state, "node-a", 100);
    state.apply(&release("node-a", 1, 1, 110), META);
    held_singleton(&mut state, "node-b", 110);

    assert_eq!(
        state.apply(&command, META),
        OwnershipEffect::Rejected(Rejection::SingletonLost)
    );
}

#[test]
fn test_singleton_grants_survive_a_snapshot_round_trip() {
    let mut state = OwnershipState::new();
    held_singleton(&mut state, "node-a", 100);

    let mut restored = OwnershipState::restore(&state.snapshot()).unwrap();

    assert_eq!(restored, state);
    assert_eq!(
        restored.apply(&acquire("node-b", 110), META),
        OwnershipEffect::SingletonHeld {
            holder: "node-a".to_owned(),
        }
    );
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
fn test_transfer_moves_the_home_and_mints_the_next_epoch() {
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
}

#[test]
fn test_successive_transfers_leave_only_the_last_home() {
    let mut state = OwnershipState::new();
    state.apply(&assign("proj", "east"), META);

    state.apply(&transfer("proj", "west"), META);
    state.apply(&transfer("proj", "north"), META);

    assert_eq!(state.epoch(&key("proj")), AuthorityEpoch(3));
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
    assert_eq!(state.home(&key("proj")), Some(&dc("east")));
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
    let snapshot = br#"{"authorities":{"proj":{"home":"east","epoch":0,"assignment":{"cause":"first-publish","term":1,"index":1,"epoch":1},"writes":{}}},"singletons":{},"controls":{}}"#;

    let error = OwnershipState::restore(snapshot).unwrap_err();

    assert!(matches!(error, OwnershipError::ZeroEpoch { authority } if authority == "proj"));
}

/// A transferred authority and an epoch-advanced one reach the same home and epoch, so the two
/// snapshots must be byte-identical. Retaining a move trail makes the transferred one grow with the
/// number of moves applied.
#[test]
fn test_repeated_transfers_add_nothing_to_the_snapshot() {
    let rounds = 1_000;
    let mut advanced = OwnershipState::new();
    advanced.apply(&assign("proj", "east"), META);
    let mut moved = OwnershipState::new();
    moved.apply(&assign("proj", "east"), META);

    for round in 0..rounds {
        advanced.apply(&advance("proj"), META);
        moved.apply(&transfer("proj", if round % 2 == 0 { "west" } else { "east" }), META);
    }

    assert_eq!(moved.home(&key("proj")), Some(&dc("east")));
    assert_eq!(moved.epoch(&key("proj")), AuthorityEpoch(rounds + 1));
    assert_eq!(moved.snapshot(), advanced.snapshot());
}

/// Snapshot cost tracks the authorities the group still homes, so forgetting one returns the exact
/// bytes it contributed rather than leaving a shrunken record behind.
#[test]
fn test_forgetting_an_authority_returns_the_snapshot_to_its_size_without_it() {
    let mut retained = OwnershipState::new();
    retained.apply(&assign("beta", "west"), META);
    let mut state = OwnershipState::new();
    state.apply(&assign("alpha", "east"), META);
    state.apply(&assign("beta", "west"), META);
    state.apply(&transfer("alpha", "north"), META);

    let effect = state.apply(&forget("alpha"), META);

    assert_eq!(
        effect,
        OwnershipEffect::Forgotten {
            epoch: AuthorityEpoch(2)
        }
    );
    assert_eq!(state.snapshot(), retained.snapshot());
}

#[test]
fn test_a_forgotten_authority_reads_as_unassigned_and_restores_without_it() {
    let mut state = OwnershipState::new();
    state.apply(&assign("alpha", "east"), META);
    state.apply(&assign("beta", "west"), META);

    state.apply(&forget("alpha"), META);
    let restored = OwnershipState::restore(&state.snapshot()).unwrap();

    assert_eq!(restored.epoch(&key("alpha")), AuthorityEpoch(0));
    assert_eq!(restored.home(&key("alpha")), None);
    assert_eq!(restored.assignment(&key("alpha")), None);
    assert_eq!(restored.home(&key("beta")), Some(&dc("west")));
}

#[test]
fn test_forgetting_an_authority_the_state_never_held_changes_nothing() {
    let mut state = OwnershipState::new();
    state.apply(&assign("beta", "west"), META);
    let unchanged = state.snapshot();

    let effect = state.apply(&forget("alpha"), META);

    assert_eq!(effect, OwnershipEffect::AlreadyForgotten);
    assert_eq!(state.snapshot(), unchanged);
}

#[test]
fn test_forgetting_is_refused_while_a_write_lease_is_live() {
    let mut state = OwnershipState::new();
    state.apply(&assign("alpha", "east"), META);
    state.apply(&begin_write("alpha", 1, "write-1", 100), META);
    let held = state.snapshot();

    let effect = state.apply(&forget_at("alpha", 100), META);

    assert_eq!(effect, OwnershipEffect::Rejected(Rejection::WritesInFlight));
    assert_eq!(state.snapshot(), held);
}

#[test]
fn test_forgetting_ignores_a_write_lease_that_has_expired() {
    let mut state = OwnershipState::new();
    state.apply(&assign("alpha", "east"), META);
    state.apply(&begin_write("alpha", 1, "write-1", 100), META);

    let lapsed = 100 + peryx_ha::AUTHORITY_WRITE_LEASE_SECS + peryx_ha::AUTHORITY_CLOCK_SKEW_SECS;
    let effect = state.apply(&forget_at("alpha", lapsed), META);

    assert_eq!(
        effect,
        OwnershipEffect::Forgotten {
            epoch: AuthorityEpoch(1)
        }
    );
}

#[test]
fn test_publishing_to_a_forgotten_authority_assigns_it_again_from_epoch_one() {
    let mut state = OwnershipState::new();
    state.apply(&assign("alpha", "east"), META);
    state.apply(&advance("alpha"), META);
    state.apply(&forget("alpha"), META);

    let effect = state.apply(&assign("alpha", "west"), META);

    assert_eq!(
        effect,
        OwnershipEffect::Assigned {
            home: dc("west"),
            epoch: AuthorityEpoch(1),
        }
    );
}
