use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use peryx_driver::state::{
    ClusterStatus, ControlCommand, ControlCommit, ControlError, HomeClaim, MembershipControl, OwnershipAuthority,
    OwnershipError, TransferOutcome,
};
use peryx_ha::PendingTransferAudit;
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;
use tokio::sync::Notify;

use super::{
    CommittedTransfers, FrontierSource, RosterFrontierSource, TransferAuditOutbox, TransferCancelError,
    TransferCoordinator, TransferDriveError, TransferRunError, commit_transfer, observe_target,
    recover_transfer_audits,
};
use crate::support::TestServer;
use crate::{
    AppliedMeta, AssignmentCause, AuthorityKey, ControlPlane, ControlResolution, DatacenterId, OwnershipCommand,
    OwnershipEffect, OwnershipState, TransferError, TransferPhase, TransferPlan, TransferRequest,
};

const BARRIER: u64 = 5;
const RETAINED: usize = 4;

fn request() -> TransferRequest {
    numbered_request("t-1", "proj")
}

fn numbered_request(id: &str, authority: &str) -> TransferRequest {
    TransferRequest {
        id: id.to_owned(),
        authority: AuthorityKey(authority.to_owned()),
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

struct Consensus {
    state: Mutex<OwnershipState>,
    index: AtomicU64,
    submissions: Mutex<Vec<ControlCommand>>,
    refusal: Mutex<Option<ControlError>>,
    unclearable: Mutex<bool>,
    unreadable: AtomicBool,
    projector: Mutex<String>,
}

impl Consensus {
    fn with_state(state: OwnershipState, next_index: u64) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(state),
            index: AtomicU64::new(next_index),
            submissions: Mutex::new(Vec::new()),
            refusal: Mutex::new(None),
            unclearable: Mutex::new(false),
            unreadable: AtomicBool::new(false),
            projector: Mutex::new("east".to_owned()),
        })
    }

    fn homed(authorities: &[&str]) -> Arc<Self> {
        let mut state = OwnershipState::new();
        for (position, authority) in authorities.iter().enumerate() {
            state.apply(
                &OwnershipCommand::AssignHome {
                    authority: AuthorityKey((*authority).to_owned()),
                    home: DatacenterId("east".to_owned()),
                    cause: AssignmentCause::FirstPublish,
                },
                AppliedMeta {
                    term: 1,
                    index: position as u64 + 1,
                },
            );
        }
        state.set_audit_projectors(BTreeSet::from(["east".to_owned(), "west".to_owned()]));
        Self::with_state(state, authorities.len() as u64 + 1)
    }

    fn refusing(error: ControlError) -> Arc<Self> {
        let consensus = Self::homed(&["proj"]);
        *consensus.refusal.lock().unwrap() = Some(error);
        consensus
    }

    fn submitted(&self) -> usize {
        self.submissions.lock().unwrap().len()
    }

    fn epoch(&self, authority: &str) -> u64 {
        self.state.lock().unwrap().epoch(&AuthorityKey(authority.to_owned())).0
    }

    fn restart(&self) -> Arc<Self> {
        Self::with_state(
            OwnershipState::restore(&self.state.lock().unwrap().snapshot()).unwrap(),
            self.index.load(Ordering::SeqCst),
        )
    }

    fn activate(&self, projector: &str) {
        *self.projector.lock().unwrap() = projector.to_owned();
    }
}

#[async_trait::async_trait]
impl MembershipControl for Consensus {
    async fn submit(&self, key: Option<&str>, command: ControlCommand) -> Result<ControlCommit, ControlError> {
        self.submissions.lock().unwrap().push(command.clone());
        let refusal = self.refusal.lock().unwrap().clone();
        if let Some(error) = refusal {
            return Err(error);
        }
        let index = self.index.fetch_add(1, Ordering::SeqCst);
        let effect = self.state.lock().unwrap().apply(
            &OwnershipCommand::AttemptControl {
                key: key.expect("a transfer identity is required").to_owned(),
                command,
                now_unix: 0,
            },
            AppliedMeta { term: 1, index },
        );
        match effect {
            OwnershipEffect::Control(ControlResolution::Committed(receipt)) => Ok(ControlCommit::committed(receipt)),
            OwnershipEffect::Control(ControlResolution::Replayed(receipt)) => Ok(ControlCommit::replayed(receipt)),
            other => Err(ControlError::Unavailable(format!("{other:?}"))),
        }
    }
}

#[async_trait::async_trait]
impl TransferAuditOutbox for Arc<Consensus> {
    async fn pending_transfer_audits(&self) -> Result<Vec<PendingTransferAudit>, OwnershipError> {
        if self.unreadable.load(Ordering::SeqCst) {
            return Err(OwnershipError::Unavailable("no leader".to_owned()));
        }
        let projector = self.projector.lock().unwrap().clone();
        Ok(self.state.lock().unwrap().pending_transfer_audits(&projector))
    }

    async fn complete_transfer_audit(&self, id: &str) -> Result<(), OwnershipError> {
        let unclearable = *self.unclearable.lock().unwrap();
        if unclearable {
            return Err(OwnershipError::Unavailable("no leader".to_owned()));
        }
        let projector = self.projector.lock().unwrap().clone();
        self.state.lock().unwrap().apply(
            &OwnershipCommand::CompleteTransferAudit {
                key: id.to_owned(),
                projector,
            },
            AppliedMeta {
                term: 1,
                index: self.index.fetch_add(1, Ordering::SeqCst),
            },
        );
        Ok(())
    }
}

fn plane(consensus: &Arc<Consensus>) -> ControlPlane {
    ControlPlane::new(consensus.clone(), Arc::new(|| 0))
}

fn meta() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, store)
}

fn unwritable(dir: &tempfile::TempDir) -> MetaStore {
    MetaStore::open_existing_read_only(dir.path().join("peryx.redb")).unwrap()
}

fn planned(request: TransferRequest) -> tokio::sync::Mutex<TransferPlan> {
    tokio::sync::Mutex::new(TransferPlan::plan(request))
}

fn ready(request: TransferRequest) -> tokio::sync::Mutex<TransferPlan> {
    let mut plan = TransferPlan::plan(request);
    assert_eq!(plan.observe_frontier(BARRIER), TransferPhase::Ready);
    tokio::sync::Mutex::new(plan)
}

#[tokio::test]
async fn test_observe_target_waits_below_the_barrier_then_readies_at_it() {
    let plan = planned(request());
    let frontier = ScriptedFrontier::new([Ok(Some(BARRIER - 1)), Ok(Some(BARRIER))]);

    assert_eq!(
        observe_target(&plan, &frontier).await.unwrap(),
        TransferPhase::AwaitingCatchUp
    );
    assert_eq!(observe_target(&plan, &frontier).await.unwrap(), TransferPhase::Ready);
}

#[tokio::test]
async fn test_observe_target_treats_an_unreachable_target_as_frontier_zero() {
    let plan = planned(request());
    let frontier = ScriptedFrontier::new([Ok(None)]);

    assert_eq!(
        observe_target(&plan, &frontier).await.unwrap(),
        TransferPhase::AwaitingCatchUp
    );
}

#[tokio::test]
async fn test_observe_target_surfaces_a_frontier_read_error() {
    let plan = planned(request());
    let frontier = ScriptedFrontier::new([Err(anyhow::anyhow!("unreachable"))]);

    let error = observe_target(&plan, &frontier).await.unwrap_err();
    assert!(matches!(error, TransferDriveError::Frontier(_)));
}

#[tokio::test]
async fn test_commit_transfer_stores_the_audit_consensus_sealed() {
    let plan = ready(request());
    let consensus = Consensus::homed(&["proj"]);
    let (_dir, store) = meta();

    let audit = commit_transfer(&plan, &plane(&consensus), &consensus, &store)
        .await
        .unwrap();

    assert_eq!((audit.epoch.0, audit.commit_term, audit.commit_index), (2, 1, 2));
    assert_eq!(audit.authority.0, "proj");
    assert_eq!(audit.target.0, "west");
    assert_eq!(consensus.submitted(), 1);
    assert_eq!(
        store.transfer_audits("proj").unwrap(),
        vec![peryx_ha::TransferAudit {
            authority: "proj".to_owned(),
            source: "east".to_owned(),
            target: "west".to_owned(),
            actor: "alice".to_owned(),
            reason: "drain east".to_owned(),
            barrier: BARRIER,
            epoch: 2,
            commit_term: 1,
            commit_index: 2,
        }]
    );
    assert!(consensus.pending_transfer_audits().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_commit_transfer_leaves_a_recovery_fact_when_the_store_write_fails() {
    let (dir, store) = meta();
    drop(store);
    let plan = ready(request());
    let consensus = Consensus::homed(&["proj"]);

    let error = commit_transfer(&plan, &plane(&consensus), &consensus, &unwritable(&dir))
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        TransferDriveError::ProjectionPending { id, .. } if id == "t-1"
    ));
    assert_eq!(
        consensus.pending_transfer_audits().await.unwrap(),
        vec![PendingTransferAudit {
            id: "t-1".to_owned(),
            audit: peryx_ha::TransferAudit {
                authority: "proj".to_owned(),
                source: "east".to_owned(),
                target: "west".to_owned(),
                actor: "alice".to_owned(),
                reason: "drain east".to_owned(),
                barrier: BARRIER,
                epoch: 2,
                commit_term: 1,
                commit_index: 2,
            },
        }]
    );
}

#[tokio::test]
async fn test_recovery_after_a_restart_stores_the_original_epoch_without_a_second_move() {
    let (dir, store) = meta();
    drop(store);
    let consensus = Consensus::homed(&["proj"]);
    let plan = ready(request());
    commit_transfer(&plan, &plane(&consensus), &consensus, &unwritable(&dir))
        .await
        .unwrap_err();
    drop(plan);
    let consensus = consensus.restart();
    let store = MetaStore::open_existing(dir.path().join("peryx.redb")).unwrap();

    let recovered = recover_transfer_audits(&consensus, &store).await.unwrap();

    assert_eq!(recovered, 1);
    assert_eq!(
        store
            .transfer_audits("proj")
            .unwrap()
            .iter()
            .map(|audit| (audit.epoch, audit.commit_term, audit.commit_index))
            .collect::<Vec<_>>(),
        vec![(2, 1, 2)]
    );
    assert_eq!(consensus.epoch("proj"), 2);
    assert!(consensus.pending_transfer_audits().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_commit_transfer_retry_by_identity_replays_the_sealed_audit() {
    let consensus = Consensus::homed(&["proj"]);
    let (_dir, store) = meta();
    let first = ready(request());
    let committed = commit_transfer(&first, &plane(&consensus), &consensus, &store)
        .await
        .unwrap();
    let retry = ready(request());

    let replayed = commit_transfer(&retry, &plane(&consensus), &consensus, &store)
        .await
        .unwrap();

    assert_eq!(replayed, committed);
    assert_eq!(consensus.epoch("proj"), 2);
    assert_eq!(store.transfer_audits("proj").unwrap().len(), 1);
}

#[tokio::test]
async fn test_commit_transfer_retry_after_projection_answers_from_the_stored_audit() {
    let consensus = Consensus::homed(&["proj"]);
    let (_dir, store) = meta();
    let first = ready(request());
    let committed = commit_transfer(&first, &plane(&consensus), &consensus, &store)
        .await
        .unwrap();
    let retry = ready(request());

    let replayed = commit_transfer(&retry, &plane(&consensus), &consensus, &store)
        .await
        .unwrap();

    assert_eq!(replayed, committed);
}

#[tokio::test]
async fn test_commit_transfer_retry_on_another_member_projects_the_sealed_receipt() {
    let consensus = Consensus::homed(&["proj"]);
    let (_dir, store) = meta();
    let first = ready(request());
    let committed = commit_transfer(&first, &plane(&consensus), &consensus, &store)
        .await
        .unwrap();
    consensus.activate("west");
    let (_other_dir, other) = meta();
    let retry = ready(request());

    let replayed = commit_transfer(&retry, &plane(&consensus), &consensus, &other)
        .await
        .unwrap();

    assert_eq!(replayed, committed);
    assert_eq!(
        other.transfer_audits("proj").unwrap()[0].commit_index,
        committed.commit_index
    );
    assert!(consensus.pending_transfer_audits().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_commit_transfer_succeeds_when_the_fact_cannot_be_cleared() {
    let consensus = Consensus::homed(&["proj"]);
    *consensus.unclearable.lock().unwrap() = true;
    let (_dir, store) = meta();
    let plan = ready(request());

    let audit = commit_transfer(&plan, &plane(&consensus), &consensus, &store)
        .await
        .unwrap();

    assert_eq!(audit.epoch.0, 2);
    assert_eq!(store.transfer_audits("proj").unwrap().len(), 1);
    assert_eq!(consensus.pending_transfer_audits().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_commit_transfer_uses_the_receipt_when_the_outbox_read_is_unavailable() {
    let consensus = Consensus::homed(&["proj"]);
    consensus.unreadable.store(true, Ordering::SeqCst);
    let (_dir, store) = meta();
    let plan = ready(request());

    let audit = commit_transfer(&plan, &plane(&consensus), &consensus, &store)
        .await
        .unwrap();

    assert_eq!(audit.commit_index, 2);
    assert_eq!(consensus.epoch("proj"), 2);
}

#[tokio::test]
async fn test_recover_transfer_audits_surfaces_an_unreadable_outbox() {
    let (_dir, store) = meta();
    let consensus = Consensus::homed(&["proj"]);
    consensus.unreadable.store(true, Ordering::SeqCst);

    let error = recover_transfer_audits(&consensus, &store).await.unwrap_err();

    assert!(matches!(error, TransferDriveError::Recover(_)));
}

#[tokio::test]
async fn test_ownership_authority_supplies_the_transfer_audit_outbox() {
    let authority: Arc<dyn OwnershipAuthority> = Arc::new(FixedAuthority(7));

    assert_eq!(
        TransferAuditOutbox::pending_transfer_audits(&authority).await.unwrap(),
        Vec::new()
    );
    assert!(
        TransferAuditOutbox::complete_transfer_audit(&authority, "t-1")
            .await
            .is_err()
    );
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
    let plan = ready(request());
    let consensus = Consensus::refusing(ControlError::NotLeader { leader: None });
    let (_dir, store) = meta();

    let error = commit_transfer(&plan, &plane(&consensus), &consensus, &store)
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        TransferDriveError::Commit(ControlError::NotLeader { .. })
    ));
    assert!(store.transfer_audits("proj").unwrap().is_empty());
    assert!(consensus.pending_transfer_audits().await.unwrap().is_empty());
    // An outcome consensus may still have appended keeps the claim, so a retry resolves the same
    // identity rather than reopening the move to cancellation.
    assert_eq!(plan.lock().await.phase(), TransferPhase::Committing);
}

#[tokio::test]
async fn test_commit_transfer_returns_a_claim_refused_before_submission_to_ready() {
    let plan = ready(request());
    let consensus = Consensus::homed(&["proj"]);
    let (_dir, store) = meta();
    let control = plane(&consensus);
    control
        .execute(
            "alice",
            Some("t-1"),
            ControlCommand::TransferAuthority {
                authority: "proj".to_owned(),
                new_home: "north".to_owned(),
                intent: None,
            },
        )
        .await
        .unwrap();

    let error = commit_transfer(&plan, &control, &consensus, &store).await.unwrap_err();

    assert!(matches!(error, TransferDriveError::Commit(ControlError::KeyReuse)));
    // The refusal came before consensus saw the move, so the identity may wait again and be cancelled.
    assert_eq!(plan.lock().await.phase(), TransferPhase::Ready);
    assert_eq!(consensus.submitted(), 1);
}

#[tokio::test]
async fn test_commit_transfer_retries_an_unresolved_claim_under_the_same_identity() {
    let plan = ready(request());
    let consensus = Consensus::refusing(ControlError::Unavailable("no quorum".to_owned()));
    let (_dir, store) = meta();
    commit_transfer(&plan, &plane(&consensus), &consensus, &store)
        .await
        .unwrap_err();
    assert_eq!(plan.lock().await.phase(), TransferPhase::Committing);
    *consensus.refusal.lock().unwrap() = None;

    let audit = commit_transfer(&plan, &plane(&consensus), &consensus, &store)
        .await
        .unwrap();

    assert_eq!(audit.id, "t-1");
    assert_eq!(consensus.epoch("proj"), 2);
    assert_eq!(store.transfer_audits("proj").unwrap().len(), 1);
}

#[tokio::test]
async fn test_commit_transfer_refuses_a_plan_that_has_not_reached_the_barrier() {
    let plan = planned(request());
    let consensus = Consensus::homed(&["proj"]);
    let (_dir, store) = meta();

    let error = commit_transfer(&plan, &plane(&consensus), &consensus, &store)
        .await
        .unwrap_err();

    assert!(matches!(error, TransferDriveError::Plan(TransferError::BarrierNotMet)));
    assert_eq!(consensus.submitted(), 0);
}

#[tokio::test]
async fn test_commit_transfer_refuses_a_cancelled_plan_without_committing() {
    let plan = ready(request());
    plan.lock().await.cancel().unwrap();
    let consensus = Consensus::homed(&["proj"]);
    let (_dir, store) = meta();

    let error = commit_transfer(&plan, &plane(&consensus), &consensus, &store)
        .await
        .unwrap_err();

    assert!(matches!(error, TransferDriveError::Plan(TransferError::Cancelled)));
    assert_eq!(consensus.submitted(), 0);
}

#[tokio::test]
async fn test_commit_transfer_replays_a_committed_plan_without_recommitting() {
    let plan = ready(request());
    let consensus = Consensus::homed(&["proj"]);
    let (_dir, store) = meta();

    let first = commit_transfer(&plan, &plane(&consensus), &consensus, &store)
        .await
        .unwrap();
    let replay = commit_transfer(&plan, &plane(&consensus), &consensus, &store)
        .await
        .unwrap();

    assert_eq!(first, replay);
    assert_eq!(consensus.submitted(), 1);
}

struct GatedFrontier {
    probed: Arc<Notify>,
    probes: AtomicUsize,
    applied: Option<u64>,
}

impl GatedFrontier {
    fn new(applied: Option<u64>) -> (Arc<Self>, Arc<Notify>) {
        let probed = Arc::new(Notify::new());
        (
            Arc::new(Self {
                probed: probed.clone(),
                probes: AtomicUsize::new(0),
                applied,
            }),
            probed,
        )
    }
}

#[async_trait::async_trait]
impl FrontierSource for GatedFrontier {
    async fn applied_frontier(&self, _datacenter: &str) -> anyhow::Result<Option<u64>> {
        self.probes.fetch_add(1, Ordering::SeqCst);
        self.probed.notify_one();
        Ok(self.applied)
    }
}

#[tokio::test]
async fn test_coordinator_drives_a_ready_transfer_to_a_sealed_audit() {
    let (frontier, _probed) = GatedFrontier::new(Some(BARRIER));
    let coordinator = TransferCoordinator::with_schedule(frontier, Duration::ZERO, 3, RETAINED);
    let (_dir, store) = meta();
    let consensus = Consensus::homed(&["proj"]);

    let audit = coordinator
        .run(request(), &plane(&consensus), &consensus, &store)
        .await
        .unwrap();

    assert_eq!((audit.epoch.0, audit.commit_term, audit.commit_index), (2, 1, 2));
    assert_eq!(consensus.submitted(), 1);
    assert_eq!(store.transfer_audits("proj").unwrap().len(), 1);
}

#[tokio::test]
async fn test_coordinator_gives_up_when_the_target_never_reaches_the_barrier() {
    let (frontier, _probed) = GatedFrontier::new(Some(BARRIER - 1));
    let coordinator = TransferCoordinator::with_schedule(frontier, Duration::ZERO, 3, RETAINED);
    let (_dir, store) = meta();
    let consensus = Consensus::homed(&["proj"]);

    let error = coordinator
        .run(request(), &plane(&consensus), &consensus, &store)
        .await
        .unwrap_err();

    assert!(matches!(error, TransferRunError::BarrierNotReached));
    assert_eq!(consensus.submitted(), 0);
    assert!(store.transfer_audits("proj").unwrap().is_empty());
}

// paused-clock-safe: the frontier source is an in-process trait implementation, so the coordinator dials nothing
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
    let consensus = Consensus::homed(&["proj"]);
    let running = tokio::spawn({
        let coordinator = coordinator.clone();
        let store = store.clone();
        let consensus = consensus.clone();
        async move { coordinator.run(request(), &plane(&consensus), &consensus, &store).await }
    });
    probed.notified().await;

    let error = coordinator
        .run(request(), &plane(&consensus), &consensus, &store)
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

// paused-clock-safe: the frontier source is an in-process trait implementation, so the coordinator dials nothing
#[tokio::test(start_paused = true)]
async fn test_coordinator_cancel_abandons_an_active_transfer() {
    let (frontier, probed) = GatedFrontier::new(Some(BARRIER - 1));
    let coordinator = Arc::new(TransferCoordinator::with_schedule(
        frontier.clone(),
        Duration::from_secs(30),
        10,
        RETAINED,
    ));
    let (_dir, store) = meta();
    let consensus = Consensus::homed(&["proj"]);
    let running = tokio::spawn({
        let coordinator = coordinator.clone();
        let store = store.clone();
        let consensus = consensus.clone();
        async move { coordinator.run(request(), &plane(&consensus), &consensus, &store).await }
    });
    probed.notified().await;

    coordinator.cancel("proj", &store).await.unwrap();
    // Letting the poll interval elapse would re-probe a coordinator that slept through the cancel.
    tokio::time::advance(Duration::from_secs(30)).await;
    assert!(matches!(
        running.await.unwrap(),
        Err(TransferRunError::Drive(TransferDriveError::Plan(
            TransferError::Cancelled
        )))
    ));
    assert_eq!(frontier.probes.load(Ordering::SeqCst), 1);
}

fn coordinator() -> TransferCoordinator {
    let (frontier, _probed) = GatedFrontier::new(Some(BARRIER));
    TransferCoordinator::with_schedule(frontier, Duration::ZERO, 3, RETAINED)
}

async fn commit(coordinator: &TransferCoordinator, store: &MetaStore, authority: &str) {
    let consensus = Consensus::homed(&[authority]);
    coordinator
        .run(
            TransferRequest {
                authority: AuthorityKey(authority.to_owned()),
                ..request()
            },
            &plane(&consensus),
            &consensus,
            store,
        )
        .await
        .unwrap();
}

async fn abandon(coordinator: &TransferCoordinator, store: &MetaStore, authority: &str) {
    let consensus = Consensus::homed(&[authority]);
    let error = coordinator
        .run(
            TransferRequest {
                authority: AuthorityKey(authority.to_owned()),
                barrier: BARRIER + 1,
                ..request()
            },
            &plane(&consensus),
            &consensus,
            store,
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

/// Parks in the probe, holding the coordinator on a frontier the way an unresponsive target does.
struct HeldFrontier {
    held: Arc<Notify>,
    release: Arc<Notify>,
    completed: AtomicUsize,
}

#[async_trait::async_trait]
impl FrontierSource for HeldFrontier {
    async fn applied_frontier(&self, _datacenter: &str) -> anyhow::Result<Option<u64>> {
        self.held.notify_one();
        self.release.notified().await;
        self.completed.fetch_add(1, Ordering::SeqCst);
        Ok(Some(BARRIER))
    }
}

#[tokio::test]
async fn test_coordinator_cancel_leaves_a_frontier_probe_rather_than_committing_behind_it() {
    let (_dir, store) = meta();
    let (held, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let frontier = Arc::new(HeldFrontier {
        held: held.clone(),
        release: release.clone(),
        completed: AtomicUsize::new(0),
    });
    let consensus = Consensus::homed(&["proj"]);
    let coordinator = Arc::new(TransferCoordinator::with_schedule(
        frontier.clone(),
        Duration::ZERO,
        1,
        RETAINED,
    ));
    let running = tokio::spawn({
        let coordinator = coordinator.clone();
        let store = store.clone();
        let consensus = consensus.clone();
        async move { coordinator.run(request(), &plane(&consensus), &consensus, &store).await }
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
    // Freeing the probe carries a coordinator that waited it out into the commit, so the assertions
    // below separate the two outcomes rather than hanging on the one that never answers.
    release.notify_one();

    cancelling.await.unwrap().unwrap();

    assert!(matches!(
        running.await.unwrap(),
        Err(TransferRunError::Drive(TransferDriveError::Plan(
            TransferError::Cancelled
        )))
    ));
    assert_eq!(frontier.completed.load(Ordering::SeqCst), 0);
    assert_eq!(consensus.submitted(), 0);
}

/// Parks in the submission so a cancellation arrives while the ownership command is in consensus.
struct HeldControl {
    consensus: Arc<Consensus>,
    submitting: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait::async_trait]
impl MembershipControl for HeldControl {
    async fn submit(&self, key: Option<&str>, command: ControlCommand) -> Result<ControlCommit, ControlError> {
        self.submitting.notify_one();
        self.release.notified().await;
        self.consensus.submit(key, command).await
    }
}

#[tokio::test]
async fn test_coordinator_cancel_of_a_claimed_commit_is_refused_while_the_command_is_in_flight() {
    let (_dir, store) = meta();
    let (submitting, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let consensus = Consensus::homed(&["proj"]);
    let held = ControlPlane::new(
        Arc::new(HeldControl {
            consensus: consensus.clone(),
            submitting: submitting.clone(),
            release: release.clone(),
        }),
        Arc::new(|| 0),
    );
    let (frontier, _probed) = GatedFrontier::new(Some(BARRIER));
    let coordinator = Arc::new(TransferCoordinator::with_schedule(
        frontier,
        Duration::ZERO,
        1,
        RETAINED,
    ));
    let running = tokio::spawn({
        let coordinator = coordinator.clone();
        let store = store.clone();
        let consensus = consensus.clone();
        async move { coordinator.run(request(), &held, &consensus, &store).await }
    });
    submitting.notified().await;
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

    // The command has not been released, so a cancel that answered read the plan rather than queuing
    // behind the consensus round trip.
    let answered_in_flight = cancelling.is_finished();
    release.notify_one();

    assert!(answered_in_flight);
    let error = cancelling.await.unwrap().unwrap_err();
    assert!(matches!(error, TransferCancelError::AlreadyCommitted(authority) if authority == "proj"));
    assert_eq!(running.await.unwrap().unwrap().commit_index, 2);
}

#[tokio::test]
async fn test_coordinator_cancel_after_an_unresolved_commit_is_not_reported_abandoned() {
    let (_dir, store) = meta();
    let (frontier, _probed) = GatedFrontier::new(Some(BARRIER));
    let coordinator = TransferCoordinator::with_schedule(frontier, Duration::ZERO, 1, RETAINED);
    let consensus = Consensus::refusing(ControlError::Unavailable("no quorum".to_owned()));
    let refused = coordinator
        .run(request(), &plane(&consensus), &consensus, &store)
        .await
        .unwrap_err();
    assert!(matches!(
        refused,
        TransferRunError::Drive(TransferDriveError::Commit(ControlError::Unavailable(_)))
    ));

    let error = coordinator.cancel("proj", &store).await.unwrap_err();

    assert!(matches!(error, TransferCancelError::Unknown(authority) if authority == "proj"));
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
