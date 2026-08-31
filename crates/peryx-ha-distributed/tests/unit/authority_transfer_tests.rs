use std::sync::{Arc, Mutex};
use std::time::Duration;

use peryx_driver::state::{
    ClusterStatus, CommandOutcome, CommandReceipt, ControlCommand, ControlCommit, ControlError, HomeClaim,
    MembershipControl, OwnershipAuthority, OwnershipError, TransferOutcome,
};
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;
use tokio::sync::Notify;

use super::{
    CommittedTransfers, EpochOracle, FrontierSource, RosterFrontierSource, TransferCancelError, TransferCoordinator,
    TransferDriveError, TransferRunError, commit_transfer, observe_target,
};
use crate::support::TestServer;
use crate::{AuthorityKey, ControlPlane, DatacenterId, TransferError, TransferPhase, TransferPlan, TransferRequest};

const BARRIER: u64 = 5;
const RETAINED: usize = 4;

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

struct FixedEpoch(u64);

#[async_trait::async_trait]
impl EpochOracle for FixedEpoch {
    async fn committed_epoch(&self, _authority: &str) -> u64 {
        self.0
    }
}

struct FixedAuthority(u64);

#[async_trait::async_trait]
impl OwnershipAuthority for FixedAuthority {
    async fn claim_home(&self, _authority: &str) -> Result<HomeClaim, OwnershipError> {
        Ok(HomeClaim {
            home: "east".to_owned(),
            epoch: self.0,
        })
    }

    fn cluster_status(&self) -> ClusterStatus {
        ClusterStatus {
            leader: None,
            term: self.0,
            voters: Vec::new(),
        }
    }

    async fn committed_epoch(&self, _authority: &str) -> u64 {
        self.0
    }

    async fn admit_epoch(&self, _authority: &str, _presented: u64) -> bool {
        true
    }

    async fn transfer_home(
        &self,
        _authority: &str,
        _new_home: &str,
    ) -> Result<Option<TransferOutcome>, OwnershipError> {
        Ok(None)
    }
}

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
    async fn submit(&self, _key: Option<&str>, command: ControlCommand) -> Result<ControlCommit, ControlError> {
        self.submissions.lock().unwrap().push(command);
        self.result
            .lock()
            .unwrap()
            .take()
            .expect("the scripted control was submitted twice")
            .map(ControlCommit::committed)
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

    assert_eq!((audit.epoch.0, audit.commit_index), (3, 9));
    assert_eq!(audit.authority.0, "proj");
    assert_eq!(audit.target.0, "west");
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
async fn test_ownership_authority_supplies_the_committed_epoch() {
    let authority: Arc<dyn OwnershipAuthority> = Arc::new(FixedAuthority(7));
    assert_eq!(EpochOracle::committed_epoch(&authority, "proj").await, 7);
}

#[tokio::test]
async fn test_fixed_authority_answers_its_snapshot() {
    let authority = FixedAuthority(7);
    assert_eq!(
        authority.claim_home("proj").await.unwrap(),
        HomeClaim {
            home: "east".to_owned(),
            epoch: 7,
        }
    );
    assert_eq!(authority.cluster_status().term, 7);
    assert_eq!(OwnershipAuthority::committed_epoch(&authority, "proj").await, 7);
    assert!(authority.admit_epoch("proj", 7).await);
    assert_eq!(authority.transfer_home("proj", "west").await.unwrap(), None);
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

    assert!(matches!(error, TransferDriveError::Plan(TransferError::BarrierNotMet)));
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

    assert!(matches!(error, TransferDriveError::Plan(TransferError::Cancelled)));
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
    let replay = commit_transfer(&mut plan, &plane, &FixedEpoch(3), &store, None)
        .await
        .unwrap();

    assert_eq!(first, replay);
    assert_eq!(scripted.submissions.lock().unwrap().len(), 1);
}

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
    let coordinator = TransferCoordinator::with_schedule(frontier, Duration::ZERO, 3, RETAINED);
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
    let coordinator = TransferCoordinator::with_schedule(frontier, Duration::ZERO, 3, RETAINED);
    let (_dir, store) = meta();
    let (scripted, plane) = control(Ok(receipt(9)));

    let error = coordinator
        .run(request(), &plane, &FixedEpoch(3), &store, None)
        .await
        .unwrap_err();

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
        RETAINED,
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

    let (_scripted, plane) = control(Ok(receipt(9)));
    let error = coordinator
        .run(request(), &plane, &FixedEpoch(3), &store, None)
        .await
        .unwrap_err();

    assert!(matches!(error, TransferRunError::Busy(authority) if authority == "proj"));
    coordinator.cancel("proj", &store).await.unwrap();
    tokio::time::advance(Duration::from_secs(30)).await;
    assert!(matches!(
        running.await.unwrap(),
        Err(TransferRunError::Drive(TransferDriveError::Plan(
            TransferError::Cancelled
        )))
    ));
}

#[tokio::test(start_paused = true)]
async fn test_coordinator_cancel_abandons_an_active_transfer() {
    let (frontier, probed) = GatedFrontier::new(Some(BARRIER - 1));
    let coordinator = Arc::new(TransferCoordinator::with_schedule(
        frontier,
        Duration::from_secs(30),
        10,
        RETAINED,
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

    coordinator.cancel("proj", &store).await.unwrap();
    tokio::time::advance(Duration::from_secs(30)).await;
    assert!(matches!(
        running.await.unwrap(),
        Err(TransferRunError::Drive(TransferDriveError::Plan(
            TransferError::Cancelled
        )))
    ));
}

fn coordinator() -> TransferCoordinator {
    let (frontier, _probed) = GatedFrontier::new(Some(BARRIER));
    TransferCoordinator::with_schedule(frontier, Duration::ZERO, 3, RETAINED)
}

async fn commit(coordinator: &TransferCoordinator, store: &MetaStore, authority: &str) {
    let (_scripted, plane) = control(Ok(receipt(9)));
    coordinator
        .run(
            TransferRequest {
                authority: AuthorityKey(authority.to_owned()),
                ..request()
            },
            &plane,
            &FixedEpoch(3),
            store,
            None,
        )
        .await
        .unwrap();
}

async fn abandon(coordinator: &TransferCoordinator, store: &MetaStore, authority: &str) {
    let (_scripted, plane) = control(Ok(receipt(9)));
    let error = coordinator
        .run(
            TransferRequest {
                authority: AuthorityKey(authority.to_owned()),
                barrier: BARRIER + 1,
                ..request()
            },
            &plane,
            &FixedEpoch(3),
            store,
            None,
        )
        .await
        .unwrap_err();
    assert!(matches!(error, TransferRunError::BarrierNotReached));
}

#[tokio::test]
async fn test_coordinator_cancel_of_a_committed_transfer_is_refused() {
    let (_dir, store) = meta();
    let coordinator = coordinator();
    commit(&coordinator, &store, "proj").await;

    let error = coordinator.cancel("proj", &store).await.unwrap_err();

    assert!(matches!(error, TransferCancelError::AlreadyCommitted(authority) if authority == "proj"));
}

/// Holds the plan lock across the commit so a cancel queued behind it resolves against a plan that
/// committed while it waited.
struct HeldFrontier {
    held: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait::async_trait]
impl FrontierSource for HeldFrontier {
    async fn applied_frontier(&self, _datacenter: &str) -> anyhow::Result<Option<u64>> {
        self.held.notify_one();
        self.release.notified().await;
        Ok(Some(BARRIER))
    }
}

#[tokio::test]
async fn test_coordinator_cancel_that_loses_the_race_to_the_commit_is_refused() {
    let (_dir, store) = meta();
    let (held, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let coordinator = Arc::new(TransferCoordinator::with_schedule(
        Arc::new(HeldFrontier {
            held: held.clone(),
            release: release.clone(),
        }),
        Duration::ZERO,
        1,
        RETAINED,
    ));
    let running = tokio::spawn({
        let coordinator = coordinator.clone();
        let store = store.clone();
        async move {
            let (_scripted, plane) = control(Ok(receipt(9)));
            coordinator.run(request(), &plane, &FixedEpoch(3), &store, None).await
        }
    });
    held.notified().await;
    let queued = Arc::new(Notify::new());
    let cancelling = tokio::spawn({
        let coordinator = coordinator.clone();
        let store = store.clone();
        let queued = queued.clone();
        async move {
            queued.notify_one();
            coordinator.cancel("proj", &store).await
        }
    });
    queued.notified().await;
    release.notify_one();

    let error = cancelling.await.unwrap().unwrap_err();

    assert!(matches!(error, TransferCancelError::AlreadyCommitted(authority) if authority == "proj"));
    assert_eq!(running.await.unwrap().unwrap().commit_index, 9);
}

/// A coordinator that never ran the move stands in for the process that restarted after it.
#[tokio::test]
async fn test_coordinator_cancel_after_a_commit_reads_the_persisted_audit() {
    let (_dir, store) = meta();
    commit(&coordinator(), &store, "proj").await;

    let error = coordinator().cancel("proj", &store).await.unwrap_err();

    assert!(matches!(error, TransferCancelError::AlreadyCommitted(authority) if authority == "proj"));
}

#[tokio::test]
async fn test_coordinator_cancel_of_an_unregistered_authority_is_unknown() {
    let (_dir, store) = meta();
    let (frontier, _probed) = GatedFrontier::new(Some(BARRIER));
    let coordinator = TransferCoordinator::new(frontier);

    let error = coordinator.cancel("ghost", &store).await.unwrap_err();

    assert!(matches!(error, TransferCancelError::Unknown(authority) if authority == "ghost"));
}

#[tokio::test]
async fn test_coordinator_cancel_of_an_abandoned_transfer_still_in_the_window_is_idempotent() {
    let (_dir, store) = meta();
    let coordinator = coordinator();
    abandon(&coordinator, &store, "proj").await;

    coordinator.cancel("proj", &store).await.unwrap();
}

#[tokio::test]
async fn test_coordinator_cancel_of_an_abandoned_transfer_evicted_from_the_window_is_unknown() {
    let (_dir, store) = meta();
    let (frontier, _probed) = GatedFrontier::new(Some(BARRIER));
    let coordinator = TransferCoordinator::with_schedule(frontier, Duration::ZERO, 3, 1);
    abandon(&coordinator, &store, "proj").await;
    abandon(&coordinator, &store, "other").await;

    let error = coordinator.cancel("proj", &store).await.unwrap_err();

    assert!(matches!(error, TransferCancelError::Unknown(authority) if authority == "proj"));
}

#[tokio::test]
async fn test_coordinator_forgets_an_abandonment_once_the_authority_moves() {
    let (_dir, store) = meta();
    let coordinator = coordinator();
    abandon(&coordinator, &store, "proj").await;
    commit(&coordinator, &store, "proj").await;

    let error = coordinator.cancel("proj", &store).await.unwrap_err();

    assert!(matches!(error, TransferCancelError::AlreadyCommitted(authority) if authority == "proj"));
}

struct UnreadableAudits;

#[async_trait::async_trait]
impl CommittedTransfers for UnreadableAudits {
    async fn committed(&self, _authority: &str) -> anyhow::Result<bool> {
        Err(anyhow::anyhow!("the metadata store is unreadable"))
    }
}

#[tokio::test]
async fn test_coordinator_cancel_surfaces_an_unreadable_durable_record() {
    let (frontier, _probed) = GatedFrontier::new(Some(BARRIER));
    let coordinator = TransferCoordinator::new(frontier);

    let error = coordinator.cancel("proj", &UnreadableAudits).await.unwrap_err();

    assert_eq!(
        error.to_string(),
        "read the durable transfer record for proj: the metadata store is unreadable"
    );
}

#[tokio::test]
async fn test_a_store_without_the_audit_table_reports_no_committed_transfer() {
    let (_dir, store) = meta();

    assert!(!CommittedTransfers::committed(&store, "proj").await.unwrap());
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
    let source = RosterFrontierSource::new(vec![("west".to_owned(), "http://127.0.0.1:1/".to_owned())], "token");

    assert_eq!(source.applied_frontier("west").await.unwrap(), None);
}

#[tokio::test]
async fn test_roster_frontier_reads_a_reachable_datacenter() {
    let (dir, meta) = meta();
    meta.commit_driver_txn(|_| Ok::<_, peryx_storage::meta::MetaError>(((), vec![b"one".to_vec()])))
        .unwrap();
    let server = TestServer::start(
        crate::primary_router("writer", "token", meta, BlobStore::new(dir.path().join("blobs"))).unwrap(),
    )
    .await;
    let source = RosterFrontierSource::new(vec![("west".to_owned(), server.url.clone())], "token");

    assert_eq!(source.applied_frontier("west").await.unwrap(), Some(1));
}
