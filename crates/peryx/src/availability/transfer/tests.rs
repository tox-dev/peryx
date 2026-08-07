use std::sync::{Arc, Mutex};
use std::time::Duration;

use peryx_driver::state::{
    CommandOutcome, CommandReceipt, ControlCommand, ControlError, ControlPlane, MembershipControl,
};
use peryx_ha_distributed::{AuthorityKey, DatacenterId, TransferPhase, TransferPlan, TransferRequest};
use peryx_storage::meta::MetaStore;
use tokio::sync::Notify;

use super::{
    EpochOracle, FrontierSource, RosterFrontierSource, TransferCancelError, TransferCoordinator, TransferDriveError,
    TransferRunError, commit_transfer, observe_target,
};

const BARRIER: u64 = 5;

fn request() -> TransferRequest {
    TransferRequest {
        authority: AuthorityKey("proj".to_owned()),
        source: DatacenterId("east".to_owned()),
        target: DatacenterId("west".to_owned()),
        actor: "alice".to_owned(),
        reason: "drain east".to_owned(),
        barrier: BARRIER,
    }
}

/// A frontier source returning one scripted answer for the target.
struct ScriptedFrontier(Mutex<Vec<anyhow::Result<Option<u64>>>>);

impl ScriptedFrontier {
    fn new(answers: impl IntoIterator<Item = anyhow::Result<Option<u64>>>) -> Self {
        let mut answers: Vec<_> = answers.into_iter().collect();
        answers.reverse();
        Self(Mutex::new(answers))
    }
}

#[async_trait::async_trait]
impl FrontierSource for ScriptedFrontier {
    async fn applied_frontier(&self, _datacenter: &str) -> anyhow::Result<Option<u64>> {
        self.0
            .lock()
            .unwrap()
            .pop()
            .expect("the scripted frontier ran out of answers")
    }
}

/// An epoch oracle returning a fixed committed epoch.
struct FixedEpoch(u64);

#[async_trait::async_trait]
impl EpochOracle for FixedEpoch {
    async fn committed_epoch(&self, _authority: &str) -> u64 {
        self.0
    }
}

/// A membership control returning one scripted result and recording its submissions.
struct ScriptedControl {
    result: Mutex<Option<Result<CommandReceipt, ControlError>>>,
    submissions: Mutex<Vec<ControlCommand>>,
}

impl ScriptedControl {
    fn new(result: Result<CommandReceipt, ControlError>) -> Arc<Self> {
        Arc::new(Self {
            result: Mutex::new(Some(result)),
            submissions: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait::async_trait]
impl MembershipControl for ScriptedControl {
    async fn submit(&self, command: ControlCommand) -> Result<CommandReceipt, ControlError> {
        self.submissions.lock().unwrap().push(command);
        self.result
            .lock()
            .unwrap()
            .take()
            .expect("the scripted control was submitted twice")
    }
}

fn receipt(index: u64) -> CommandReceipt {
    CommandReceipt {
        term: 1,
        index,
        outcome: CommandOutcome::Committed,
        old_voters: Vec::new(),
        new_voters: Vec::new(),
    }
}

fn control(result: Result<CommandReceipt, ControlError>) -> (Arc<ScriptedControl>, ControlPlane) {
    let scripted = ScriptedControl::new(result);
    let plane = ControlPlane::new(scripted.clone(), Arc::new(|| 0));
    (scripted, plane)
}

fn meta() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, store)
}

#[tokio::test]
async fn test_observe_target_waits_below_the_barrier_then_readies_at_it() {
    let mut plan = TransferPlan::plan(request());
    let frontier = ScriptedFrontier::new([Ok(Some(BARRIER - 1)), Ok(Some(BARRIER))]);

    assert_eq!(
        observe_target(&mut plan, &frontier).await.unwrap(),
        TransferPhase::AwaitingCatchUp
    );
    assert_eq!(
        observe_target(&mut plan, &frontier).await.unwrap(),
        TransferPhase::Ready
    );
}

#[tokio::test]
async fn test_observe_target_treats_an_unreachable_target_as_frontier_zero() {
    let mut plan = TransferPlan::plan(request());
    let frontier = ScriptedFrontier::new([Ok(None)]);

    // A target that answers with no frontier never reaches the barrier, so the move keeps waiting.
    assert_eq!(
        observe_target(&mut plan, &frontier).await.unwrap(),
        TransferPhase::AwaitingCatchUp
    );
}

#[tokio::test]
async fn test_observe_target_surfaces_a_frontier_read_error() {
    let mut plan = TransferPlan::plan(request());
    let frontier = ScriptedFrontier::new([Err(anyhow::anyhow!("unreachable"))]);

    let error = observe_target(&mut plan, &frontier).await.unwrap_err();
    assert!(matches!(error, TransferDriveError::Frontier(_)));
}

#[tokio::test]
async fn test_commit_transfer_commits_derives_the_epoch_and_persists_the_audit() {
    let mut plan = TransferPlan::plan(request());
    assert_eq!(plan.observe_frontier(BARRIER), TransferPhase::Ready);
    let (scripted, plane) = control(Ok(receipt(9)));
    let (_dir, store) = meta();

    let audit = commit_transfer(&mut plan, &plane, &FixedEpoch(3), &store, Some("k1"))
        .await
        .unwrap();

    // The audit carries the committed log index and the epoch read back from committed state.
    assert_eq!((audit.epoch.0, audit.commit_index), (3, 9));
    assert_eq!(audit.authority.0, "proj");
    assert_eq!(audit.target.0, "west");
    // The move was submitted once as a transfer command, and the audit is durable.
    assert_eq!(scripted.submissions.lock().unwrap().len(), 1);
    let persisted = store.transfer_audits("proj").unwrap();
    assert_eq!(persisted.len(), 1);
    assert_eq!(
        (
            persisted[0].epoch,
            persisted[0].commit_index,
            persisted[0].reason.as_str()
        ),
        (3, 9, "drain east")
    );
}

#[tokio::test]
async fn test_commit_transfer_surfaces_a_refused_commit_without_persisting() {
    let mut plan = TransferPlan::plan(request());
    plan.observe_frontier(BARRIER);
    let (_scripted, plane) = control(Err(ControlError::NotLeader { leader: None }));
    let (_dir, store) = meta();

    let error = commit_transfer(&mut plan, &plane, &FixedEpoch(3), &store, None)
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        TransferDriveError::Commit(ControlError::NotLeader { .. })
    ));
    assert!(store.transfer_audits("proj").unwrap().is_empty());
}

#[tokio::test]
async fn test_commit_transfer_refuses_a_plan_that_has_not_reached_the_barrier() {
    let mut plan = TransferPlan::plan(request());
    let (scripted, plane) = control(Ok(receipt(9)));
    let (_dir, store) = meta();

    let error = commit_transfer(&mut plan, &plane, &FixedEpoch(3), &store, None)
        .await
        .unwrap_err();

    // A plan still awaiting catch-up never reaches consensus, so no move commits.
    assert!(matches!(
        error,
        TransferDriveError::Plan(peryx_ha_distributed::TransferError::BarrierNotMet)
    ));
    assert!(scripted.submissions.lock().unwrap().is_empty());
}

#[tokio::test]
async fn test_commit_transfer_refuses_a_cancelled_plan_without_committing() {
    let mut plan = TransferPlan::plan(request());
    plan.observe_frontier(BARRIER);
    plan.cancel().unwrap();
    let (scripted, plane) = control(Ok(receipt(9)));
    let (_dir, store) = meta();

    let error = commit_transfer(&mut plan, &plane, &FixedEpoch(3), &store, None)
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        TransferDriveError::Plan(peryx_ha_distributed::TransferError::Cancelled)
    ));
    assert!(scripted.submissions.lock().unwrap().is_empty());
}

#[tokio::test]
async fn test_commit_transfer_replays_a_committed_plan_without_recommitting() {
    let mut plan = TransferPlan::plan(request());
    plan.observe_frontier(BARRIER);
    let (scripted, plane) = control(Ok(receipt(9)));
    let (_dir, store) = meta();

    let first = commit_transfer(&mut plan, &plane, &FixedEpoch(3), &store, None)
        .await
        .unwrap();
    // A second drive of the same committed plan reads back the sealed audit without a second submission.
    let replay = commit_transfer(&mut plan, &plane, &FixedEpoch(3), &store, None)
        .await
        .unwrap();

    assert_eq!(first, replay);
    assert_eq!(scripted.submissions.lock().unwrap().len(), 1);
}

/// A frontier returning one fixed applied serial, signalling every probe so a test can wait for the
/// coordinator to register and observe before it acts.
struct GatedFrontier {
    probed: Arc<Notify>,
    applied: Option<u64>,
}

impl GatedFrontier {
    fn new(applied: Option<u64>) -> (Arc<Self>, Arc<Notify>) {
        let probed = Arc::new(Notify::new());
        (
            Arc::new(Self {
                probed: probed.clone(),
                applied,
            }),
            probed,
        )
    }
}

#[async_trait::async_trait]
impl FrontierSource for GatedFrontier {
    async fn applied_frontier(&self, _datacenter: &str) -> anyhow::Result<Option<u64>> {
        self.probed.notify_one();
        Ok(self.applied)
    }
}

#[tokio::test]
async fn test_coordinator_drives_a_ready_transfer_to_a_sealed_audit() {
    let (frontier, _probed) = GatedFrontier::new(Some(BARRIER));
    let coordinator = TransferCoordinator::with_schedule(frontier, Duration::ZERO, 3);
    let (_dir, store) = meta();
    let (scripted, plane) = control(Ok(receipt(9)));

    let audit = coordinator
        .run(request(), &plane, &FixedEpoch(3), &store, Some("k1"))
        .await
        .unwrap();

    assert_eq!((audit.epoch.0, audit.commit_index), (3, 9));
    assert_eq!(scripted.submissions.lock().unwrap().len(), 1);
    assert_eq!(store.transfer_audits("proj").unwrap().len(), 1);
}

#[tokio::test]
async fn test_coordinator_gives_up_when_the_target_never_reaches_the_barrier() {
    let (frontier, _probed) = GatedFrontier::new(Some(BARRIER - 1));
    let coordinator = TransferCoordinator::with_schedule(frontier, Duration::ZERO, 3);
    let (_dir, store) = meta();
    let (scripted, plane) = control(Ok(receipt(9)));

    let error = coordinator
        .run(request(), &plane, &FixedEpoch(3), &store, None)
        .await
        .unwrap_err();

    // The budget runs out below the barrier, so no move commits and no audit is booked.
    assert!(matches!(error, TransferRunError::BarrierNotReached));
    assert!(scripted.submissions.lock().unwrap().is_empty());
    assert!(store.transfer_audits("proj").unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn test_coordinator_refuses_a_second_transfer_for_the_same_authority() {
    let (frontier, probed) = GatedFrontier::new(Some(BARRIER - 1));
    let coordinator = Arc::new(TransferCoordinator::with_schedule(
        frontier,
        Duration::from_secs(30),
        10,
    ));
    let (_dir, store) = meta();
    let running = tokio::spawn({
        let coordinator = coordinator.clone();
        let store = store.clone();
        async move {
            let (_scripted, plane) = control(Ok(receipt(9)));
            coordinator.run(request(), &plane, &FixedEpoch(3), &store, None).await
        }
    });
    probed.notified().await;

    // The first transfer is registered and waiting, so a second for the same authority is refused.
    let (_scripted, plane) = control(Ok(receipt(9)));
    let error = coordinator
        .run(request(), &plane, &FixedEpoch(3), &store, None)
        .await
        .unwrap_err();

    assert!(matches!(error, TransferRunError::Busy(authority) if authority == "proj"));
    running.abort();
}

#[tokio::test(start_paused = true)]
async fn test_coordinator_cancel_abandons_an_active_transfer() {
    let (frontier, probed) = GatedFrontier::new(Some(BARRIER - 1));
    let coordinator = Arc::new(TransferCoordinator::with_schedule(
        frontier,
        Duration::from_secs(30),
        10,
    ));
    let (_dir, store) = meta();
    let running = tokio::spawn({
        let coordinator = coordinator.clone();
        let store = store.clone();
        async move {
            let (_scripted, plane) = control(Ok(receipt(9)));
            coordinator.run(request(), &plane, &FixedEpoch(3), &store, None).await
        }
    });
    probed.notified().await;

    // The transfer waits below its barrier, so a cancel abandons it before it can commit.
    coordinator.cancel("proj").await.unwrap();

    running.abort();
}

#[tokio::test]
async fn test_coordinator_cancel_of_a_committed_transfer_is_refused() {
    let (frontier, _probed) = GatedFrontier::new(Some(BARRIER));
    let coordinator = TransferCoordinator::with_schedule(frontier, Duration::ZERO, 3);
    let (_dir, store) = meta();
    let (_scripted, plane) = control(Ok(receipt(9)));
    coordinator
        .run(request(), &plane, &FixedEpoch(3), &store, None)
        .await
        .unwrap();

    // The committed plan stays registered, so a cancel after the move is refused rather than lost.
    let error = coordinator.cancel("proj").await.unwrap_err();

    assert!(matches!(error, TransferCancelError::AlreadyCommitted(authority) if authority == "proj"));
}

#[tokio::test]
async fn test_coordinator_cancel_of_an_unregistered_authority_is_unknown() {
    let (frontier, _probed) = GatedFrontier::new(Some(BARRIER));
    let coordinator = TransferCoordinator::with_schedule(frontier, Duration::ZERO, 3);

    let error = coordinator.cancel("ghost").await.unwrap_err();

    assert!(matches!(error, TransferCancelError::Unknown(authority) if authority == "ghost"));
}

#[tokio::test]
async fn test_roster_frontier_has_no_frontier_for_an_unknown_datacenter() {
    let source = RosterFrontierSource::new(vec![("east".to_owned(), "http://east.example/".to_owned())], "token");

    assert_eq!(source.applied_frontier("west").await.unwrap(), None);
}

#[tokio::test]
async fn test_roster_frontier_rejects_a_datacenter_with_an_unusable_address() {
    let source = RosterFrontierSource::new(vec![("west".to_owned(), "not a url".to_owned())], "token");

    assert!(source.applied_frontier("west").await.is_err());
}

#[tokio::test]
async fn test_roster_frontier_treats_an_unreachable_datacenter_as_no_frontier() {
    // Port 1 refuses the connection at once, so the probe fails at the transport and the barrier gate
    // keeps the move waiting rather than committing to a target it could not read.
    let source = RosterFrontierSource::new(vec![("west".to_owned(), "http://127.0.0.1:1/".to_owned())], "token");

    assert_eq!(source.applied_frontier("west").await.unwrap(), None);
}
