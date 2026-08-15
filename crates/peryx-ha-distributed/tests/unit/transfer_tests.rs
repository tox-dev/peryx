use crate::authority::AuthorityKey;
use crate::envelope::AuthorityEpoch;
use crate::ownership::DatacenterId;
use crate::transfer::{TransferAudit, TransferError, TransferPhase, TransferPlan, TransferRequest};

fn request(barrier: u64) -> TransferRequest {
    TransferRequest {
        authority: AuthorityKey("root/alpha/resource-a".to_owned()),
        source: DatacenterId("dc-east".to_owned()),
        target: DatacenterId("dc-west".to_owned()),
        actor: "alice".to_owned(),
        reason: "decommission dc-east".to_owned(),
        barrier,
    }
}

fn ready_plan(barrier: u64) -> TransferPlan {
    let mut plan = TransferPlan::plan(request(barrier));
    plan.observe_frontier(barrier);
    plan
}

fn sealed_audit(barrier: u64, epoch: u64, commit_index: u64) -> TransferAudit {
    TransferAudit {
        authority: AuthorityKey("root/alpha/resource-a".to_owned()),
        source: DatacenterId("dc-east".to_owned()),
        target: DatacenterId("dc-west".to_owned()),
        actor: "alice".to_owned(),
        reason: "decommission dc-east".to_owned(),
        barrier,
        epoch: AuthorityEpoch(epoch),
        commit_index,
    }
}

#[test]
fn test_a_new_plan_awaits_catch_up() {
    let plan = TransferPlan::plan(request(10));
    assert_eq!(plan.phase(), TransferPhase::AwaitingCatchUp);
    assert_eq!(plan.audit(), None);
    assert_eq!(plan.request(), &request(10));
}

#[test]
fn test_a_frontier_short_of_the_barrier_keeps_the_plan_waiting() {
    let mut plan = TransferPlan::plan(request(10));
    assert_eq!(plan.observe_frontier(9), TransferPhase::AwaitingCatchUp);
}

#[test]
fn test_a_frontier_at_the_barrier_readies_the_plan() {
    let mut plan = TransferPlan::plan(request(10));
    assert_eq!(plan.observe_frontier(10), TransferPhase::Ready);
}

#[test]
fn test_a_later_lower_frontier_never_un_readies_a_plan() {
    let mut plan = ready_plan(10);
    assert_eq!(plan.observe_frontier(0), TransferPhase::Ready);
}

#[test]
fn test_committing_a_ready_plan_seals_the_full_audit() {
    let mut plan = ready_plan(10);
    let audit = plan.commit(AuthorityEpoch(4), 128).unwrap();
    assert_eq!(audit, sealed_audit(10, 4, 128));
    assert_eq!(plan.phase(), TransferPhase::Committed);
    assert_eq!(plan.audit(), Some(&sealed_audit(10, 4, 128)));
}

#[test]
fn test_committing_before_the_barrier_is_refused() {
    let mut plan = TransferPlan::plan(request(10));
    assert_eq!(plan.commit(AuthorityEpoch(4), 128), Err(TransferError::BarrierNotMet));
    assert_eq!(plan.phase(), TransferPhase::AwaitingCatchUp);
}

#[test]
fn test_committing_twice_books_one_outcome() {
    let mut plan = ready_plan(10);
    let first = plan.commit(AuthorityEpoch(4), 128).unwrap();
    let second = plan.commit(AuthorityEpoch(9), 256).unwrap();
    assert_eq!(first, second);
}

#[test]
fn test_a_cancelled_plan_cannot_commit() {
    let mut plan = TransferPlan::plan(request(10));
    plan.cancel().unwrap();
    assert_eq!(plan.commit(AuthorityEpoch(4), 128), Err(TransferError::Cancelled));
}

#[test]
fn test_cancelling_a_waiting_plan_abandons_it() {
    let mut plan = TransferPlan::plan(request(10));
    plan.cancel().unwrap();
    assert_eq!(plan.phase(), TransferPhase::Cancelled);
}

#[test]
fn test_cancelling_a_ready_plan_abandons_it() {
    let mut plan = ready_plan(10);
    plan.cancel().unwrap();
    assert_eq!(plan.phase(), TransferPhase::Cancelled);
}

#[test]
fn test_cancelling_twice_is_a_no_op() {
    let mut plan = TransferPlan::plan(request(10));
    plan.cancel().unwrap();
    assert_eq!(plan.cancel(), Ok(()));
    assert_eq!(plan.phase(), TransferPhase::Cancelled);
}

#[test]
fn test_a_committed_plan_refuses_a_late_cancel() {
    let mut plan = ready_plan(10);
    plan.commit(AuthorityEpoch(4), 128).unwrap();
    assert_eq!(plan.cancel(), Err(TransferError::AlreadyCommitted));
    assert_eq!(plan.phase(), TransferPhase::Committed);
}

#[test]
fn test_an_observation_after_commit_does_not_regress_the_phase() {
    let mut plan = ready_plan(10);
    plan.commit(AuthorityEpoch(4), 128).unwrap();
    assert_eq!(plan.observe_frontier(0), TransferPhase::Committed);
}

#[test]
fn test_transfer_errors_render_a_distinct_reason_each() {
    assert_eq!(
        TransferError::BarrierNotMet.to_string(),
        "the target has not reached the transfer barrier"
    );
    assert_eq!(
        TransferError::AlreadyCommitted.to_string(),
        "the transfer already committed and cannot be cancelled"
    );
    assert_eq!(
        TransferError::Cancelled.to_string(),
        "the transfer was cancelled and cannot commit"
    );
}
