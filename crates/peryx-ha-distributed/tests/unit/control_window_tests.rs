//! The replicated idempotency window, exercised through committed ownership commands.

use crate::authority::AuthorityKey;
use crate::envelope::AuthorityEpoch;
use crate::ownership::{
    AppliedMeta, AssignmentCause, ControlRejection, ControlResolution, DatacenterId, OwnershipCommand, OwnershipEffect,
    OwnershipState, Rejection, control_outcome,
};
use peryx_ha::{
    CONTROL_IDEMPOTENCY_SECS, CommandOutcome, CommandReceipt, ControlCommand, PendingTransferAudit, TransferAudit,
    TransferIntent,
};
use rstest::rstest;

const META: AppliedMeta = AppliedMeta { term: 4, index: 9 };

fn key(name: &str) -> AuthorityKey {
    AuthorityKey(name.to_owned())
}

fn assign(authority: &str) -> OwnershipCommand {
    OwnershipCommand::AssignHome {
        authority: key(authority),
        home: DatacenterId("east".to_owned()),
        cause: AssignmentCause::FirstPublish,
    }
}

fn advance_epoch() -> ControlCommand {
    ControlCommand::AdvanceEpoch {
        authority: "proj".to_owned(),
    }
}

const BARRIER: u64 = 5;

fn intent() -> TransferIntent {
    TransferIntent {
        source: "east".to_owned(),
        actor: "alice".to_owned(),
        reason: "drain east".to_owned(),
        barrier: BARRIER,
    }
}

fn move_home(new_home: &str) -> ControlCommand {
    ControlCommand::TransferAuthority {
        authority: "proj".to_owned(),
        new_home: new_home.to_owned(),
        intent: Some(intent()),
    }
}

fn sealed(new_home: &str, epoch: u64) -> TransferAudit {
    TransferAudit {
        authority: "proj".to_owned(),
        source: "east".to_owned(),
        target: new_home.to_owned(),
        actor: "alice".to_owned(),
        reason: "drain east".to_owned(),
        barrier: BARRIER,
        epoch,
        commit_term: META.term,
        commit_index: META.index,
    }
}

fn forget_authority() -> ControlCommand {
    ControlCommand::ForgetAuthority {
        authority: "proj".to_owned(),
    }
}

fn add_learner() -> ControlCommand {
    ControlCommand::AddLearner {
        datacenter: "west".to_owned(),
        address: "http://west.internal:4460".to_owned(),
    }
}

fn promote() -> ControlCommand {
    ControlCommand::PromoteVoter {
        datacenter: "west".to_owned(),
    }
}

fn attempt_at(name: &str, command: ControlCommand, now_unix: i64) -> OwnershipCommand {
    OwnershipCommand::AttemptControl {
        key: name.to_owned(),
        command,
        now_unix,
    }
}

fn attempt(name: &str, command: ControlCommand) -> OwnershipCommand {
    attempt_at(name, command, 0)
}

fn complete(name: &str) -> OwnershipCommand {
    OwnershipCommand::CompleteTransferAudit { key: name.to_owned() }
}

fn release(name: &str, command: ControlCommand) -> OwnershipCommand {
    OwnershipCommand::ReleaseControl {
        key: name.to_owned(),
        command,
        now_unix: 0,
    }
}

fn settle_at(name: &str, receipt: CommandReceipt, now_unix: i64) -> OwnershipCommand {
    OwnershipCommand::SettleControl {
        key: name.to_owned(),
        command: add_learner(),
        receipt,
        now_unix,
    }
}

/// The receipt [`META`] stamps on an authority command, which carries no voter transition.
fn authority_receipt(outcome: CommandOutcome) -> CommandReceipt {
    CommandReceipt {
        term: META.term,
        index: META.index,
        outcome,
        old_voters: Vec::new(),
        new_voters: Vec::new(),
    }
}

fn membership_receipt(index: u64) -> CommandReceipt {
    CommandReceipt {
        term: 4,
        index,
        outcome: CommandOutcome::Committed,
        old_voters: vec!["east".to_owned()],
        new_voters: vec!["east".to_owned()],
    }
}

fn resolved(resolution: ControlResolution) -> OwnershipEffect {
    OwnershipEffect::Control(resolution)
}

fn homed() -> OwnershipState {
    let mut state = OwnershipState::new();
    state.apply(&assign("proj"), META);
    state
}

fn leased() -> OwnershipState {
    let mut state = homed();
    state.apply(
        &OwnershipCommand::BeginEpochWrite {
            authority: key("proj"),
            epoch: AuthorityEpoch(1),
            id: "write-1".to_owned(),
            issued_at_unix: 0,
            expires_at_unix: peryx_ha::AUTHORITY_WRITE_LEASE_SECS,
        },
        META,
    );
    state
}

#[test]
fn test_an_epoch_advance_records_its_receipt_in_the_deciding_entry() {
    let mut state = homed();

    let effect = state.apply(&attempt("k1", advance_epoch()), META);

    assert_eq!(
        effect,
        resolved(ControlResolution::Committed(authority_receipt(
            CommandOutcome::Committed
        )))
    );
    assert_eq!(state.epoch(&key("proj")), AuthorityEpoch(2));
}

#[test]
fn test_a_repeated_epoch_advance_replays_without_advancing_again() {
    let mut state = homed();
    state.apply(&attempt("k1", advance_epoch()), META);

    let repeated = state.apply(&attempt("k1", advance_epoch()), AppliedMeta { term: 5, index: 20 });

    assert_eq!(
        repeated,
        resolved(ControlResolution::Replayed(authority_receipt(
            CommandOutcome::Committed
        ))),
        "the retry answers with the term and index of the entry that mutated"
    );
    assert_eq!(
        state.epoch(&key("proj")),
        AuthorityEpoch(2),
        "a replacement leader answering a retry mutates nothing"
    );
}

#[test]
fn test_a_transfer_to_the_current_home_seals_the_epoch_it_resolved_against() {
    let mut state = homed();

    let effect = state.apply(&attempt("k1", move_home("east")), META);

    assert_eq!(
        effect,
        resolved(ControlResolution::Committed(authority_receipt(
            CommandOutcome::NoChange
        )))
    );
    assert_eq!(
        state.pending_transfer_audits(),
        vec![PendingTransferAudit {
            id: "k1".to_owned(),
            audit: sealed("east", 1),
        }]
    );
}

#[test]
fn test_a_transfer_records_its_receipt_moves_the_home_and_seals_its_audit() {
    let mut state = homed();

    let effect = state.apply(&attempt("k1", move_home("west")), META);

    assert_eq!(
        effect,
        resolved(ControlResolution::Committed(authority_receipt(
            CommandOutcome::Committed
        )))
    );
    assert_eq!(state.home(&key("proj")), Some(&DatacenterId("west".to_owned())));
    assert_eq!(
        state.pending_transfer_audits(),
        vec![PendingTransferAudit {
            id: "k1".to_owned(),
            audit: sealed("west", 2),
        }]
    );
}

#[test]
fn test_a_rejected_transfer_seals_no_audit() {
    let mut state = OwnershipState::new();

    let effect = state.apply(&attempt("k1", move_home("west")), META);

    assert_eq!(
        effect,
        resolved(ControlResolution::Rejected(ControlRejection::NotAssigned))
    );
    assert_eq!(state.pending_transfer_audits(), Vec::new());
}

#[test]
fn test_a_repeated_transfer_replays_the_sealed_audit_without_moving_again() {
    let mut state = homed();
    state.apply(&attempt("k1", move_home("west")), META);

    let repeated = state.apply(&attempt("k1", move_home("west")), AppliedMeta { term: 5, index: 20 });

    assert_eq!(
        repeated,
        resolved(ControlResolution::Replayed(authority_receipt(
            CommandOutcome::Committed
        )))
    );
    assert_eq!(state.epoch(&key("proj")), AuthorityEpoch(2));
}

#[test]
fn test_an_unprojected_audit_outlives_the_idempotency_window() {
    let mut state = homed();
    state.apply(&attempt_at("k1", move_home("west"), 0), META);

    state.apply(&attempt_at("k2", advance_epoch(), CONTROL_IDEMPOTENCY_SECS), META);

    assert_eq!(
        state.pending_transfer_audits(),
        vec![PendingTransferAudit {
            id: "k1".to_owned(),
            audit: sealed("west", 2),
        }]
    );
}

#[test]
fn test_an_unprojected_audit_survives_a_snapshot_round_trip() {
    let mut state = homed();
    state.apply(&attempt("k1", move_home("west")), META);

    let restored = OwnershipState::restore(&state.snapshot()).unwrap();

    assert_eq!(
        restored.pending_transfer_audits(),
        vec![PendingTransferAudit {
            id: "k1".to_owned(),
            audit: sealed("west", 2),
        }]
    );
}

#[test]
fn test_completing_a_projection_drops_the_audit_and_keeps_the_receipt() {
    let mut state = homed();
    state.apply(&attempt("k1", move_home("west")), META);

    let completed = state.apply(&complete("k1"), META);
    let replayed = state.apply(&attempt("k1", move_home("west")), META);

    assert_eq!(completed, OwnershipEffect::TransferAuditCompleted);
    assert_eq!(state.pending_transfer_audits(), Vec::new());
    assert_eq!(
        replayed,
        resolved(ControlResolution::Replayed(authority_receipt(
            CommandOutcome::Committed
        )))
    );
}

#[test]
fn test_completing_an_unknown_projection_changes_nothing() {
    let mut state = homed();

    let completed = state.apply(&complete("ghost"), META);

    assert_eq!(completed, OwnershipEffect::TransferAuditCompleted);
    assert_eq!(state.pending_transfer_audits(), Vec::new());
}

#[test]
fn test_a_projected_key_prunes_with_the_window() {
    let mut state = homed();
    state.apply(&attempt_at("k1", move_home("west"), 0), META);
    state.apply(&complete("k1"), META);

    let reopened = state.apply(&attempt_at("k1", move_home("east"), CONTROL_IDEMPOTENCY_SECS), META);

    assert_eq!(
        reopened,
        resolved(ControlResolution::Committed(authority_receipt(
            CommandOutcome::Committed
        ))),
        "the pruned key stands for whatever the next attempt carries"
    );
}

#[test]
fn test_forgetting_a_homed_authority_records_its_receipt_and_drops_the_record() {
    let mut state = homed();

    let effect = state.apply(&attempt("k1", forget_authority()), META);

    assert_eq!(
        effect,
        resolved(ControlResolution::Committed(authority_receipt(
            CommandOutcome::Committed
        )))
    );
    assert_eq!(state.epoch(&key("proj")), AuthorityEpoch(0));
}

#[test]
fn test_forgetting_an_authority_the_group_never_homed_records_a_no_change_receipt() {
    let mut state = OwnershipState::new();

    let effect = state.apply(&attempt("k1", forget_authority()), META);

    assert_eq!(
        effect,
        resolved(ControlResolution::Committed(authority_receipt(
            CommandOutcome::NoChange
        )))
    );
}

#[rstest]
#[case::unassigned(OwnershipState::new(), ControlRejection::NotAssigned)]
#[case::leased(leased(), ControlRejection::WritesInFlight)]
fn test_a_rejected_mutation_records_nothing(#[case] mut state: OwnershipState, #[case] expected: ControlRejection) {
    let effect = state.apply(&attempt("k1", advance_epoch()), META);

    assert_eq!(effect, resolved(ControlResolution::Rejected(expected)));
}

#[test]
fn test_a_rejected_key_stays_open_to_a_later_attempt() {
    let mut state = OwnershipState::new();
    state.apply(&attempt("k1", advance_epoch()), META);

    state.apply(&assign("proj"), META);
    let retried = state.apply(&attempt("k1", advance_epoch()), META);

    assert_eq!(
        retried,
        resolved(ControlResolution::Committed(authority_receipt(
            CommandOutcome::Committed
        )))
    );
    assert_eq!(state.epoch(&key("proj")), AuthorityEpoch(2));
}

#[test]
fn test_a_key_bound_to_a_different_command_is_refused() {
    let mut state = homed();
    state.apply(&attempt("k1", advance_epoch()), META);

    let reused = state.apply(&attempt("k1", move_home("west")), META);

    assert_eq!(reused, resolved(ControlResolution::KeyReuse));
    assert_eq!(state.home(&key("proj")), Some(&DatacenterId("east".to_owned())));
}

#[test]
fn test_a_membership_command_is_claimed_and_leaves_ownership_alone() {
    let mut state = homed();

    let effect = state.apply(&attempt("k1", add_learner()), META);

    assert_eq!(effect, resolved(ControlResolution::Claimed));
    assert_eq!(state.epoch(&key("proj")), AuthorityEpoch(1));
}

#[test]
fn test_an_unsettled_membership_claim_is_reclaimed_by_a_retry() {
    let mut state = OwnershipState::new();
    state.apply(&attempt("k1", add_learner()), META);

    let retried = state.apply(&attempt("k1", add_learner()), META);

    assert_eq!(
        retried,
        resolved(ControlResolution::Claimed),
        "a membership change converges, so an unanswered claim is re-run rather than stranded"
    );
}

#[test]
fn test_a_membership_claim_bound_to_another_command_is_refused_before_it_settles() {
    let mut state = OwnershipState::new();
    state.apply(&attempt("k1", add_learner()), META);

    let reused = state.apply(&attempt("k1", promote()), META);

    assert_eq!(reused, resolved(ControlResolution::KeyReuse));
}

#[test]
fn test_a_settled_membership_claim_replays_its_receipt() {
    let mut state = OwnershipState::new();
    state.apply(&attempt("k1", add_learner()), META);
    let settled = state.apply(&settle_at("k1", membership_receipt(11), 0), META);

    let replayed = state.apply(&attempt("k1", add_learner()), META);

    assert_eq!(settled, OwnershipEffect::ControlSettled(membership_receipt(11)));
    assert_eq!(replayed, resolved(ControlResolution::Replayed(membership_receipt(11))));
}

#[test]
fn test_settlement_keeps_the_receipt_the_caller_was_already_given() {
    let mut state = OwnershipState::new();
    state.apply(&attempt("k1", add_learner()), META);
    state.apply(&settle_at("k1", membership_receipt(11), 0), META);

    let second = state.apply(&settle_at("k1", membership_receipt(12), 0), META);

    assert_eq!(second, OwnershipEffect::ControlSettled(membership_receipt(11)));
}

#[test]
fn test_a_settlement_of_a_pruned_claim_rebinds_the_key() {
    let mut state = OwnershipState::new();
    state.apply(&attempt("k1", add_learner()), META);

    let settled = state.apply(&settle_at("k1", membership_receipt(11), CONTROL_IDEMPOTENCY_SECS), META);
    let replayed = state.apply(&attempt_at("k1", add_learner(), CONTROL_IDEMPOTENCY_SECS), META);

    assert_eq!(settled, OwnershipEffect::ControlSettled(membership_receipt(11)));
    assert_eq!(replayed, resolved(ControlResolution::Replayed(membership_receipt(11))));
}

#[test]
fn test_a_released_claim_leaves_its_key_open_to_a_different_command() {
    let mut state = OwnershipState::new();
    state.apply(&attempt("k1", add_learner()), META);

    let released = state.apply(&release("k1", add_learner()), META);
    let reused = state.apply(&attempt("k1", promote()), META);

    assert_eq!(released, OwnershipEffect::ControlReleased);
    assert_eq!(
        reused,
        resolved(ControlResolution::Claimed),
        "a failed attempt holds nothing, so the key stands for whatever the retry carries"
    );
}

#[test]
fn test_a_release_keeps_the_receipt_a_retry_already_settled() {
    let mut state = OwnershipState::new();
    state.apply(&attempt("k1", add_learner()), META);
    state.apply(&settle_at("k1", membership_receipt(11), 0), META);

    state.apply(&release("k1", add_learner()), META);

    assert_eq!(
        state.apply(&attempt("k1", add_learner()), META),
        resolved(ControlResolution::Replayed(membership_receipt(11)))
    );
}

#[test]
fn test_a_release_leaves_a_claim_bound_to_another_command_alone() {
    let mut state = OwnershipState::new();
    state.apply(&attempt("k1", add_learner()), META);

    state.apply(&release("k1", promote()), META);

    assert_eq!(
        state.apply(&attempt("k1", promote()), META),
        resolved(ControlResolution::KeyReuse),
        "the claim the release named had already been rebound, so it is not the one to free"
    );
}

#[test]
fn test_releasing_an_unbound_key_binds_nothing() {
    let mut state = OwnershipState::new();

    let released = state.apply(&release("k1", add_learner()), META);

    assert_eq!(released, OwnershipEffect::ControlReleased);
    assert_eq!(
        state.apply(&attempt("k1", promote()), META),
        resolved(ControlResolution::Claimed)
    );
}

#[test]
fn test_a_released_key_stays_open_across_a_snapshot_round_trip() {
    let mut state = OwnershipState::new();
    state.apply(&attempt("k1", add_learner()), META);
    state.apply(&release("k1", add_learner()), META);

    let mut restored = OwnershipState::restore(&state.snapshot()).unwrap();

    assert_eq!(
        restored.apply(&attempt("k1", promote()), META),
        resolved(ControlResolution::Claimed)
    );
}

#[rstest]
#[case::inside_the_window(
    CONTROL_IDEMPOTENCY_SECS - 1,
    2,
    ControlResolution::Replayed(authority_receipt(CommandOutcome::Committed))
)]
#[case::past_the_window(
    CONTROL_IDEMPOTENCY_SECS,
    3,
    ControlResolution::Committed(authority_receipt(CommandOutcome::Committed))
)]
fn test_a_key_is_replayable_until_its_window_closes(
    #[case] elapsed: i64,
    #[case] epoch: u64,
    #[case] expected: ControlResolution,
) {
    let mut state = homed();
    state.apply(&attempt_at("k1", advance_epoch(), 0), META);

    let effect = state.apply(&attempt_at("k1", advance_epoch(), elapsed), META);

    assert_eq!(effect, resolved(expected));
    assert_eq!(state.epoch(&key("proj")), AuthorityEpoch(epoch));
}

#[test]
fn test_pruning_one_key_leaves_a_younger_key_replayable() {
    let mut state = homed();
    state.apply(&attempt_at("old", advance_epoch(), 0), META);
    state.apply(&attempt_at("young", advance_epoch(), 10), META);

    let young = state.apply(&attempt_at("young", advance_epoch(), CONTROL_IDEMPOTENCY_SECS), META);

    assert_eq!(
        young,
        resolved(ControlResolution::Replayed(authority_receipt(
            CommandOutcome::Committed
        )))
    );
}

#[test]
fn test_the_window_survives_a_snapshot_round_trip() {
    let mut state = homed();
    state.apply(&attempt("k1", advance_epoch()), META);

    let mut restored = OwnershipState::restore(&state.snapshot()).unwrap();
    let replayed = restored.apply(&attempt("k1", advance_epoch()), META);

    assert_eq!(
        replayed,
        resolved(ControlResolution::Replayed(authority_receipt(
            CommandOutcome::Committed
        )))
    );
    assert_eq!(restored.epoch(&key("proj")), AuthorityEpoch(2));
}

#[rstest]
#[case::advanced(OwnershipEffect::EpochAdvanced { epoch: AuthorityEpoch(2) }, Ok(CommandOutcome::Committed))]
#[case::same_home(OwnershipEffect::Rejected(Rejection::SameHome), Ok(CommandOutcome::NoChange))]
#[case::forgotten(OwnershipEffect::Forgotten { epoch: AuthorityEpoch(3) }, Ok(CommandOutcome::Committed))]
#[case::already_forgotten(OwnershipEffect::AlreadyForgotten, Ok(CommandOutcome::NoChange))]
#[case::unassigned(
    OwnershipEffect::Rejected(Rejection::NotAssigned),
    Err(ControlRejection::NotAssigned)
)]
#[case::leased(
    OwnershipEffect::Rejected(Rejection::WritesInFlight),
    Err(ControlRejection::WritesInFlight)
)]
fn test_control_outcome_maps_each_authority_effect(
    #[case] effect: OwnershipEffect,
    #[case] expected: Result<CommandOutcome, ControlRejection>,
) {
    assert_eq!(control_outcome(&effect), expected);
}
