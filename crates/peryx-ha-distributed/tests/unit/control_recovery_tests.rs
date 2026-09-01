//! Idempotency across the failures that end a request: a lost response, an aborted caller, a process
//! that never comes back. Each retry is served by a `ControlPlane` with no memory of the first attempt,
//! which is what a replacement leader or a restarted process starts out as.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use crate::consensus_runtime::{OwnershipGroup, applied_outcome, applied_receipt, applied_resolution, voter_id};
use crate::raft::log_store::RaftLogStoreAdapter;
use crate::raft::network::{PeerRaftNetworkFactory, raft_rpc_router};
use crate::raft::persistence::RaftLogStore;
use crate::raft::{OwnershipResponse, OwnershipStateMachine, PeryxNode, RaftConfig, RaftNode};
use crate::{ControlPlane, DatacenterId, OwnershipEffect};
use peryx_core::Clock;
use peryx_ha::{
    CommandOutcome, CommandReceipt, ControlCommand, ControlCommit, ControlError, MembershipControl,
    OwnershipAuthority as _,
};
use rstest::rstest;
use tempfile::TempDir;
use tokio::sync::Notify;

const TOKEN: &str = "group-secret";

fn clock() -> Clock {
    Arc::new(|| 1_700_000_000)
}

fn advance() -> ControlCommand {
    ControlCommand::AdvanceEpoch {
        authority: "proj".to_owned(),
    }
}

fn add_learner() -> ControlCommand {
    ControlCommand::AddLearner {
        datacenter: "west".to_owned(),
        address: "http://west.internal:4470".to_owned(),
    }
}

async fn leader_over(store: RaftLogStore) -> RaftNode {
    let node = RaftNode::start(
        voter_id("east"),
        RaftConfig::default(),
        "ownership",
        PeerRaftNetworkFactory::new(TOKEN, Duration::from_secs(1)),
        RaftLogStoreAdapter::new(store),
        OwnershipStateMachine::default(),
    )
    .await
    .unwrap();
    node.bootstrap(BTreeMap::from([(
        voter_id("east"),
        PeryxNode {
            datacenter: DatacenterId("east".to_owned()),
            endpoint: "http://east.internal:4460/".to_owned(),
        },
    )]))
    .await
    .unwrap();
    let mut metrics = node.metrics();
    tokio::time::timeout(
        Duration::from_secs(5),
        metrics.wait_for(|metrics| metrics.current_leader.is_some()),
    )
    .await
    .unwrap()
    .unwrap();
    node
}

fn group_over(node: &RaftNode) -> Arc<OwnershipGroup> {
    Arc::new(OwnershipGroup::new(node.clone(), DatacenterId("east".to_owned())).with_clock(clock()))
}

/// A group that has already published `proj`, so an epoch advance has something to move.
async fn homed_group(store: RaftLogStore) -> (RaftNode, Arc<OwnershipGroup>) {
    let node = leader_over(store).await;
    let group = group_over(&node);
    assert_eq!(group.claim_home("proj").await.unwrap().epoch, 1);
    (node, group)
}

async fn stop(node: RaftNode, group: Arc<OwnershipGroup>) {
    node.raft().shutdown().await.unwrap();
    drop(group);
    drop(node);
}

fn store(dir: &TempDir) -> RaftLogStore {
    RaftLogStore::open(dir.path().join("raft.redb")).unwrap()
}

/// A caller that never learns the outcome: the group commits, then the answer stalls until the caller
/// is aborted. `before_commit` stalls the submission instead, so the entry never reaches the log.
struct InterruptedControl {
    inner: Arc<OwnershipGroup>,
    before_commit: bool,
    reached: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait::async_trait]
impl MembershipControl for InterruptedControl {
    async fn submit(&self, key: Option<&str>, command: ControlCommand) -> Result<ControlCommit, ControlError> {
        if self.before_commit {
            self.reached.notify_one();
            self.release.notified().await;
        }
        let result = self.inner.submit(key, command).await;
        if !self.before_commit {
            self.reached.notify_one();
            self.release.notified().await;
        }
        result
    }
}

#[tokio::test]
async fn test_a_committed_receipt_replays_to_a_process_that_never_saw_it() {
    let dir = tempfile::tempdir().unwrap();
    let (_node, group) = homed_group(store(&dir)).await;
    let first = ControlPlane::new(group.clone(), clock())
        .execute("alice", Some("k1"), advance())
        .await
        .unwrap();

    let replacement = ControlPlane::new(group.clone(), clock())
        .execute("bob", Some("k1"), advance())
        .await
        .unwrap();

    assert_eq!(replacement, first);
    assert_eq!(group.committed_epoch("proj").await, 2, "the retry advanced nothing");
}

#[tokio::test]
async fn test_a_receipt_outlives_the_node_that_recorded_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);
    let (node, group) = homed_group(store.clone()).await;
    let first = ControlPlane::new(group.clone(), clock())
        .execute("alice", Some("k1"), advance())
        .await
        .unwrap();
    stop(node, group).await;

    let node = leader_over(store).await;
    let restarted = group_over(&node);
    let replayed = ControlPlane::new(restarted.clone(), clock())
        .execute("alice", Some("k1"), advance())
        .await
        .unwrap();

    assert_eq!(replayed, first);
    assert_eq!(restarted.committed_epoch("proj").await, 2);
}

#[tokio::test]
async fn test_a_key_reused_for_different_content_is_refused_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);
    let (node, group) = homed_group(store.clone()).await;
    ControlPlane::new(group.clone(), clock())
        .execute("alice", Some("k1"), advance())
        .await
        .unwrap();
    stop(node, group).await;

    let node = leader_over(store).await;
    let restarted = group_over(&node);
    let reused = ControlPlane::new(restarted.clone(), clock())
        .execute(
            "alice",
            Some("k1"),
            ControlCommand::TransferAuthority {
                authority: "proj".to_owned(),
                new_home: "west".to_owned(),
                intent: None,
            },
        )
        .await;

    assert_eq!(reused, Err(ControlError::KeyReuse));
    assert_eq!(restarted.committed_epoch("proj").await, 2);
}

#[rstest]
#[case::during_submission(true)]
#[case::before_receipt_delivery(false)]
#[tokio::test]
async fn test_an_interrupted_advance_leaves_exactly_one_committed_epoch(#[case] before_commit: bool) {
    let dir = tempfile::tempdir().unwrap();
    let (_node, group) = homed_group(store(&dir)).await;
    let reached = Arc::new(Notify::new());
    let interrupted = Arc::new(ControlPlane::new(
        Arc::new(InterruptedControl {
            inner: group.clone(),
            before_commit,
            reached: reached.clone(),
            release: Arc::new(Notify::new()),
        }),
        clock(),
    ));
    let caller = tokio::spawn({
        let plane = interrupted.clone();
        async move { plane.execute("alice", Some("k1"), advance()).await }
    });
    reached.notified().await;
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());

    let receipt = ControlPlane::new(group.clone(), clock())
        .execute("alice", Some("k1"), advance())
        .await
        .unwrap();

    assert_eq!(receipt.outcome, CommandOutcome::Committed);
    assert_eq!(
        group.committed_epoch("proj").await,
        2,
        "the abandoned attempt and the retry together advance the epoch once"
    );
}

#[tokio::test]
async fn test_a_membership_command_settles_its_key_for_a_later_retry() {
    let dir = tempfile::tempdir().unwrap();
    let (_node, group) = homed_group(store(&dir)).await;
    let first = ControlPlane::new(group.clone(), clock())
        .execute("alice", Some("k1"), add_learner())
        .await
        .unwrap();

    let replayed = ControlPlane::new(group.clone(), clock())
        .execute("bob", Some("k1"), add_learner())
        .await
        .unwrap();

    assert_eq!(replayed, first);
    assert_eq!(group.cluster_status().voters, ["east"], "a learner is not a voter");
}

/// An address the roster refuses, so the attempt fails after its key is claimed and before consensus
/// is asked to change anything.
fn malformed_learner() -> ControlCommand {
    ControlCommand::AddLearner {
        datacenter: "west".to_owned(),
        address: "west.internal:4470".to_owned(),
    }
}

fn refused_address() -> ControlError {
    ControlError::Invalid("member address \"west.internal:4470\" must use the http or https scheme".to_owned())
}

/// Promoting a datacenter the roster has never seen is refused by consensus rather than by validation.
fn promote_unknown() -> ControlCommand {
    ControlCommand::PromoteVoter {
        datacenter: "south".to_owned(),
    }
}

#[tokio::test]
async fn test_a_failed_membership_command_frees_its_key_for_a_corrected_retry() {
    let dir = tempfile::tempdir().unwrap();
    let (_node, group) = homed_group(store(&dir)).await;
    let refused = ControlPlane::new(group.clone(), clock())
        .execute("alice", Some("k1"), malformed_learner())
        .await;

    let corrected = ControlPlane::new(group.clone(), clock())
        .execute("alice", Some("k1"), add_learner())
        .await
        .unwrap();

    assert_eq!(refused, Err(refused_address()));
    assert_eq!(corrected.outcome, CommandOutcome::Committed);
}

#[tokio::test]
async fn test_each_failure_under_one_key_frees_it_again() {
    let dir = tempfile::tempdir().unwrap();
    let (_node, group) = homed_group(store(&dir)).await;
    ControlPlane::new(group.clone(), clock())
        .execute("alice", Some("k1"), malformed_learner())
        .await
        .unwrap_err();

    let rejected = ControlPlane::new(group.clone(), clock())
        .execute("alice", Some("k1"), promote_unknown())
        .await;
    let corrected = ControlPlane::new(group.clone(), clock())
        .execute("alice", Some("k1"), add_learner())
        .await
        .unwrap();

    assert!(matches!(rejected, Err(ControlError::Unavailable(_))));
    assert_eq!(corrected.outcome, CommandOutcome::Committed);
}

#[tokio::test]
async fn test_a_freed_key_stays_free_after_the_node_that_freed_it_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);
    let (node, group) = homed_group(store.clone()).await;
    ControlPlane::new(group.clone(), clock())
        .execute("alice", Some("k1"), malformed_learner())
        .await
        .unwrap_err();
    stop(node, group).await;

    let node = leader_over(store).await;
    let restarted = group_over(&node);
    let corrected = ControlPlane::new(restarted.clone(), clock())
        .execute("alice", Some("k1"), add_learner())
        .await
        .unwrap();

    assert_eq!(corrected.outcome, CommandOutcome::Committed);
}

type Server = tokio::task::JoinHandle<std::io::Result<()>>;

/// A live three-voter group over mounted RPC endpoints, so shutting a leader down elects a real
/// replacement instead of standing one in.
async fn live_group(dirs: &[TempDir]) -> (Vec<RaftNode>, Vec<Server>) {
    let mut nodes = Vec::new();
    let mut servers = Vec::new();
    let mut roster = BTreeMap::new();
    for (index, dir) in dirs.iter().enumerate() {
        let id = u64::try_from(index).unwrap() + 1;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let node = RaftNode::start(
            id,
            RaftConfig::default(),
            "ownership",
            PeerRaftNetworkFactory::new(TOKEN, Duration::from_secs(1)),
            RaftLogStoreAdapter::new(RaftLogStore::open(dir.path().join("raft.redb")).unwrap()),
            OwnershipStateMachine::default(),
        )
        .await
        .unwrap();
        servers.push(tokio::spawn(std::future::IntoFuture::into_future(axum::serve(
            listener,
            raft_rpc_router(id, TOKEN, node.rpc_handler()).unwrap(),
        ))));
        roster.insert(
            id,
            PeryxNode {
                datacenter: DatacenterId(format!("dc{id}")),
                endpoint: format!("http://{address}/"),
            },
        );
        nodes.push(node);
    }
    nodes[0].bootstrap(roster).await.unwrap();
    (nodes, servers)
}

/// Waits until `node` follows a leader that is not `excluded`, and names it.
async fn leader_other_than(node: &RaftNode, excluded: Option<u64>) -> u64 {
    let mut metrics = node.metrics();
    tokio::time::timeout(
        Duration::from_secs(10),
        metrics.wait_for(|metrics| matches!(metrics.current_leader, Some(leader) if Some(leader) != excluded)),
    )
    .await
    .unwrap()
    .unwrap()
    .current_leader
    .unwrap()
}

fn group_on(node: &RaftNode, id: u64) -> Arc<OwnershipGroup> {
    Arc::new(
        OwnershipGroup::new(node.clone(), DatacenterId(format!("dc{id}")))
            .with_peer_forwarding(TOKEN)
            .with_clock(clock()),
    )
}

#[tokio::test]
async fn test_a_key_freed_by_a_lost_leader_is_free_for_its_replacement() {
    let dirs: Vec<TempDir> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let (nodes, servers) = live_group(&dirs).await;
    let elected = leader_other_than(&nodes[0], None).await;
    let index = usize::try_from(elected).unwrap() - 1;
    let refused = ControlPlane::new(group_on(&nodes[index], elected), clock())
        .execute("alice", Some("k1"), malformed_learner())
        .await;
    nodes[index].raft().shutdown().await.unwrap();

    let survivor = (0..3).find(|candidate| *candidate != index).unwrap();
    let replacement = leader_other_than(&nodes[survivor], Some(elected)).await;
    let corrected = ControlPlane::new(
        group_on(&nodes[usize::try_from(replacement).unwrap() - 1], replacement),
        clock(),
    )
    .execute("alice", Some("k1"), add_learner())
    .await
    .unwrap();

    assert_eq!(refused, Err(refused_address()));
    assert_eq!(corrected.outcome, CommandOutcome::Committed);
    for (_, node) in nodes.iter().enumerate().filter(|(position, _)| *position != index) {
        node.raft().shutdown().await.unwrap();
    }
    for server in servers {
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }
}

#[tokio::test]
async fn test_a_keyed_command_the_authority_rejects_leaves_its_key_open() {
    let dir = tempfile::tempdir().unwrap();
    let node = leader_over(store(&dir)).await;
    let group = group_over(&node);
    let refused = ControlPlane::new(group.clone(), clock())
        .execute("alice", Some("k1"), advance())
        .await;

    assert_eq!(group.claim_home("proj").await.unwrap().epoch, 1);
    let committed = ControlPlane::new(group.clone(), clock())
        .execute("alice", Some("k1"), advance())
        .await
        .unwrap();

    assert_eq!(
        refused,
        Err(ControlError::Invalid(
            "the authority is not assigned a home to move or fence".to_owned()
        ))
    );
    assert_eq!(committed.outcome, CommandOutcome::Committed);
    assert_eq!(group.committed_epoch("proj").await, 2);
}

fn unapplied(subject: &str) -> ControlError {
    ControlError::Unavailable(format!("the {subject} committed without applying"))
}

fn settled() -> OwnershipResponse {
    OwnershipResponse::Applied(OwnershipEffect::ControlSettled(CommandReceipt {
        term: 1,
        index: 1,
        outcome: CommandOutcome::Committed,
        old_voters: Vec::new(),
        new_voters: Vec::new(),
    }))
}

#[test]
fn test_an_entry_that_commits_without_applying_is_unavailable() {
    assert_eq!(
        applied_outcome(OwnershipResponse::NonMutating),
        Err(unapplied("authority command"))
    );
    assert_eq!(
        applied_resolution(OwnershipResponse::NonMutating),
        Err(unapplied("control claim"))
    );
    assert_eq!(
        applied_receipt(OwnershipResponse::NonMutating),
        Err(unapplied("control settlement"))
    );
}

#[test]
fn test_a_control_answer_of_the_wrong_shape_is_unavailable() {
    assert_eq!(applied_resolution(settled()), Err(unapplied("control claim")));
    assert_eq!(
        applied_receipt(OwnershipResponse::Applied(OwnershipEffect::WriteFinished)),
        Err(unapplied("control settlement"))
    );
}
