use std::collections::BTreeMap;
use std::time::Duration;

use axum::body::Bytes;
use openraft::error::{ClientWriteError, Fatal, ForwardToLeader, InitializeError, RaftError};
use openraft::raft::{ClientWriteResponse, InstallSnapshotRequest};
use openraft::{ServerState, SnapshotMeta, Vote};
use tempfile::TempDir;

use crate::consensus_runtime::OwnershipGroup;
use crate::ownership::{AssignmentCause, DatacenterId, OwnershipCommand, OwnershipEffect, Rejection};
use crate::raft::log_store::RaftLogStoreAdapter;
use crate::raft::network::{PeerRaftNetworkFactory, RaftRpc, RaftRpcRejection, raft_rpc_router};
use crate::raft::persistence::RaftLogStore;
use crate::raft::{OwnershipResponse, OwnershipStateMachine, PeryxNode, RaftConfig, RaftNode, StartError, TypeConfig};
use crate::{AuthorityEpoch, AuthorityKey};
use peryx_ha::OwnershipAuthority as _;

const RPC_TIMEOUT: Duration = Duration::from_secs(1);

fn peer(dc: &str, endpoint: &str) -> PeryxNode {
    PeryxNode {
        datacenter: DatacenterId(dc.to_owned()),
        endpoint: endpoint.to_owned(),
    }
}

fn seed_roster() -> BTreeMap<u64, PeryxNode> {
    BTreeMap::from([(1, peer("east", "http://localhost:4460/"))])
}

async fn start_node(dir: &TempDir, config: RaftConfig) -> Result<RaftNode, StartError> {
    let store = RaftLogStore::open(dir.path().join("raft.redb")).unwrap();
    RaftNode::start(
        1,
        config,
        "ownership",
        PeerRaftNetworkFactory::new("group-secret", RPC_TIMEOUT),
        RaftLogStoreAdapter::new(store),
        OwnershipStateMachine::default(),
    )
    .await
}

async fn leader_node(dir: &TempDir) -> RaftNode {
    let node = start_node(dir, RaftConfig::default()).await.unwrap();
    node.bootstrap(seed_roster()).await.unwrap();
    node.raft()
        .wait(Some(Duration::from_secs(5)))
        .state(ServerState::Leader, "single node elects itself")
        .await
        .unwrap();
    node
}

#[tokio::test]
async fn test_a_bootstrapped_single_node_becomes_its_own_leader() {
    let dir = tempfile::tempdir().unwrap();
    let node = leader_node(&dir).await;

    assert_eq!(node.leader(), Some((1, peer("east", "http://localhost:4460/"))));
    assert_eq!(node.metrics().borrow().current_leader, Some(1));
}

#[tokio::test]
async fn test_the_leader_lookup_skips_a_non_leader_member() {
    let dir = tempfile::tempdir().unwrap();
    let node = leader_node(&dir).await;

    node.raft()
        .add_learner(0, peer("west", "localhost:4470"), false)
        .await
        .unwrap();

    assert_eq!(node.leader(), Some((1, peer("east", "http://localhost:4460/"))));
}

#[tokio::test]
async fn test_a_committed_command_returns_its_applied_effect() {
    let dir = tempfile::tempdir().unwrap();
    let node = leader_node(&dir).await;

    let response = node
        .submit(OwnershipCommand::AssignHome {
            authority: AuthorityKey("proj".to_owned()),
            home: DatacenterId("east".to_owned()),
            cause: AssignmentCause::FirstPublish,
        })
        .await
        .unwrap();

    assert!(matches!(
        response,
        OwnershipResponse::Applied(OwnershipEffect::Assigned { .. })
    ));
    node.raft().shutdown().await.unwrap();
}

#[tokio::test]
async fn test_client_write_rpc_stamps_lease_times_with_the_leader_clock() {
    let dir = tempfile::tempdir().unwrap();
    let node = leader_node(&dir).await;
    node.submit(OwnershipCommand::AssignHome {
        authority: AuthorityKey("proj".to_owned()),
        home: DatacenterId("east".to_owned()),
        cause: AssignmentCause::FirstPublish,
    })
    .await
    .unwrap();
    let handler = node.rpc_handler_with_clock(std::sync::Arc::new(|| 42));

    let body = Bytes::from(
        serde_json::to_vec(&OwnershipCommand::BeginEpochWrite {
            authority: AuthorityKey("proj".to_owned()),
            epoch: AuthorityEpoch(1),
            id: "write-1".to_owned(),
            issued_at_unix: -1_000,
            expires_at_unix: i64::MAX,
        })
        .unwrap(),
    );
    let reply = handler.handle(RaftRpc::ClientWrite, body).await.unwrap();
    let response: Result<ClientWriteResponse<TypeConfig>, RaftError<u64, ClientWriteError<u64, PeryxNode>>> =
        serde_json::from_slice(&reply).unwrap();
    assert_eq!(
        response.unwrap().data,
        OwnershipResponse::Applied(OwnershipEffect::WriteLeased {
            epoch: AuthorityEpoch(1),
            id: "write-1".to_owned(),
            expires_at_unix: 42 + peryx_ha::AUTHORITY_WRITE_LEASE_SECS,
        })
    );

    let body = Bytes::from(
        serde_json::to_vec(&OwnershipCommand::RecordTransfer {
            authority: AuthorityKey("proj".to_owned()),
            new_home: DatacenterId("west".to_owned()),
            now_unix: i64::MAX,
        })
        .unwrap(),
    );
    let reply = handler.handle(RaftRpc::ClientWrite, body).await.unwrap();
    let response: Result<ClientWriteResponse<TypeConfig>, RaftError<u64, ClientWriteError<u64, PeryxNode>>> =
        serde_json::from_slice(&reply).unwrap();
    assert_eq!(
        response.unwrap().data,
        OwnershipResponse::Applied(OwnershipEffect::Rejected(Rejection::WritesInFlight))
    );

    let body = Bytes::from(
        serde_json::to_vec(&OwnershipCommand::AdvanceAuthorityEpoch {
            authority: AuthorityKey("proj".to_owned()),
            now_unix: i64::MAX,
        })
        .unwrap(),
    );
    let reply = handler.handle(RaftRpc::ClientWrite, body).await.unwrap();
    let response: Result<ClientWriteResponse<TypeConfig>, RaftError<u64, ClientWriteError<u64, PeryxNode>>> =
        serde_json::from_slice(&reply).unwrap();
    assert_eq!(
        response.unwrap().data,
        OwnershipResponse::Applied(OwnershipEffect::Rejected(Rejection::WritesInFlight))
    );

    let body = Bytes::from(
        serde_json::to_vec(&OwnershipCommand::ForgetAuthority {
            authority: AuthorityKey("proj".to_owned()),
            now_unix: i64::MAX,
        })
        .unwrap(),
    );
    let reply = handler.handle(RaftRpc::ClientWrite, body).await.unwrap();
    let response: Result<ClientWriteResponse<TypeConfig>, RaftError<u64, ClientWriteError<u64, PeryxNode>>> =
        serde_json::from_slice(&reply).unwrap();
    assert_eq!(
        response.unwrap().data,
        OwnershipResponse::Applied(OwnershipEffect::Rejected(Rejection::WritesInFlight)),
        "a forget carrying its own far-future clock would have expired the lease and removed the record"
    );
}

#[tokio::test]
async fn test_client_write_rpc_stamps_singleton_lease_times_with_the_leader_clock() {
    let dir = tempfile::tempdir().unwrap();
    let node = leader_node(&dir).await;
    let handler = node.rpc_handler_with_clock(std::sync::Arc::new(|| 42));

    let acquired = forward(
        handler.as_ref(),
        OwnershipCommand::AcquireSingletonLease {
            job: "reclamation".to_owned(),
            holder: "node-a".to_owned(),
            now_unix: -1_000,
            expires_at_unix: i64::MAX,
        },
    )
    .await;
    assert_eq!(
        acquired,
        OwnershipResponse::Applied(OwnershipEffect::SingletonAcquired {
            holder: "node-a".to_owned(),
            term: 1,
            generation: 1,
            expires_at_unix: 42 + peryx_ha::SINGLETON_LEASE_SECS,
        })
    );

    let renewed = forward(
        handler.as_ref(),
        OwnershipCommand::RenewSingletonLease {
            job: "reclamation".to_owned(),
            holder: "node-a".to_owned(),
            term: 1,
            generation: 1,
            now_unix: i64::MIN,
            expires_at_unix: i64::MAX,
        },
    )
    .await;
    assert_eq!(
        renewed,
        OwnershipResponse::Applied(OwnershipEffect::SingletonRenewed {
            expires_at_unix: 42 + peryx_ha::SINGLETON_LEASE_SECS,
        })
    );

    let released = forward(
        handler.as_ref(),
        OwnershipCommand::ReleaseSingletonLease {
            job: "reclamation".to_owned(),
            holder: "node-a".to_owned(),
            term: 1,
            generation: 1,
            now_unix: i64::MAX,
        },
    )
    .await;
    assert_eq!(released, OwnershipResponse::Applied(OwnershipEffect::SingletonReleased));
}

async fn forward(handler: &dyn crate::raft::network::RaftRpcHandler, command: OwnershipCommand) -> OwnershipResponse {
    let body = Bytes::from(serde_json::to_vec(&command).unwrap());
    let reply = handler.handle(RaftRpc::ClientWrite, body).await.unwrap();
    let response: Result<ClientWriteResponse<TypeConfig>, RaftError<u64, ClientWriteError<u64, PeryxNode>>> =
        serde_json::from_slice(&reply).unwrap();
    response.unwrap().data
}

#[tokio::test]
async fn test_a_submit_without_a_leader_surfaces_the_error() {
    let dir = tempfile::tempdir().unwrap();
    let node = start_node(&dir, RaftConfig::default()).await.unwrap();

    let result = node
        .submit(OwnershipCommand::AdvanceAuthorityEpoch {
            authority: AuthorityKey("proj".to_owned()),
            now_unix: 0,
        })
        .await;

    assert!(result.is_err());
}

#[tokio::test]
async fn test_bootstrap_is_idempotent_across_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let node = leader_node(&dir).await;

    node.bootstrap(seed_roster()).await.unwrap();
}

#[tokio::test]
async fn test_bootstrap_rejects_a_roster_that_omits_this_node() {
    let dir = tempfile::tempdir().unwrap();
    let node = start_node(&dir, RaftConfig::default()).await.unwrap();

    let result = node
        .bootstrap(BTreeMap::from([(2, peer("west", "localhost:4470"))]))
        .await;

    assert!(matches!(
        result,
        Err(RaftError::APIError(InitializeError::NotInMembers(_)))
    ));
}

#[tokio::test]
async fn test_a_fresh_node_reports_no_leader() {
    let dir = tempfile::tempdir().unwrap();
    let node = start_node(&dir, RaftConfig::default()).await.unwrap();

    assert_eq!(node.leader(), None);
}

#[tokio::test]
async fn test_an_invalid_tuning_fails_to_start() {
    let dir = tempfile::tempdir().unwrap();
    let inverted = RaftConfig {
        election_timeout_min: Duration::from_millis(600),
        election_timeout_max: Duration::from_millis(300),
        ..RaftConfig::default()
    };

    let result = start_node(&dir, inverted).await;

    assert!(matches!(result, Err(StartError::Config(_))));
}

#[tokio::test]
async fn test_a_corrupt_log_store_fails_to_start() {
    let dir = tempfile::tempdir().unwrap();
    let store = RaftLogStore::open(dir.path().join("raft.redb")).unwrap();
    store.save_vote(b"not valid json").unwrap();
    let result = RaftNode::start(
        1,
        RaftConfig::default(),
        "ownership",
        PeerRaftNetworkFactory::new("group-secret", RPC_TIMEOUT),
        RaftLogStoreAdapter::new(store),
        OwnershipStateMachine::default(),
    )
    .await;

    assert!(matches!(result, Err(StartError::Fatal(_))));
}

#[tokio::test]
async fn test_forward_target_prefers_the_error_leader_over_local_metrics() {
    let dir = tempfile::tempdir().unwrap();
    let node = leader_node(&dir).await;

    let stamped = peer("west", "localhost:4470");
    let error = RaftError::APIError(ClientWriteError::ForwardToLeader(ForwardToLeader {
        leader_id: Some(2),
        leader_node: Some(stamped.clone()),
    }));

    assert_eq!(node.forward_target(&error), Some((2, stamped)));
}

#[tokio::test]
async fn test_forward_target_falls_back_to_local_metrics_without_a_stamped_leader() {
    let dir = tempfile::tempdir().unwrap();
    let node = leader_node(&dir).await;

    let error = RaftError::APIError(ClientWriteError::ForwardToLeader(ForwardToLeader {
        leader_id: None,
        leader_node: None,
    }));

    assert_eq!(node.forward_target(&error), node.leader());
}

#[tokio::test]
async fn test_forward_target_falls_back_for_a_non_forwarding_error() {
    let dir = tempfile::tempdir().unwrap();
    let node = leader_node(&dir).await;

    let error = RaftError::Fatal(Fatal::Stopped);

    assert_eq!(node.forward_target(&error), node.leader());
}

#[tokio::test]
async fn test_a_reassignment_returns_its_claim_through_the_shared_state() {
    let dir = tempfile::tempdir().unwrap();
    let node = leader_node(&dir).await;
    let assign = OwnershipCommand::AssignHome {
        authority: AuthorityKey("proj".to_owned()),
        home: DatacenterId("east".to_owned()),
        cause: AssignmentCause::FirstPublish,
    };
    node.submit(assign.clone()).await.unwrap();

    let response = node.submit(assign).await.unwrap();
    assert_eq!(
        response,
        OwnershipResponse::Applied(OwnershipEffect::AlreadyAssigned {
            home: DatacenterId("east".to_owned()),
            epoch: AuthorityEpoch(1),
        })
    );

    node.raft().ensure_linearizable().await.unwrap();
    let _ = node.state_machine();
}

#[tokio::test]
async fn test_a_three_node_group_forms_quorum_over_the_mounted_rpc_endpoints() {
    const CLUSTER_TOKEN: &str = "cluster-secret";
    let mut nodes = Vec::new();
    let mut roster = BTreeMap::new();
    let mut guards = Vec::new();
    let mut servers = Vec::new();
    for id in 1..=3u64 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let guard = tempfile::tempdir().unwrap();
        let store = RaftLogStore::open(guard.path().join("raft.redb")).unwrap();
        let node = RaftNode::start(
            id,
            RaftConfig::default(),
            "ownership",
            PeerRaftNetworkFactory::new(CLUSTER_TOKEN, RPC_TIMEOUT),
            RaftLogStoreAdapter::new(store),
            OwnershipStateMachine::default(),
        )
        .await
        .unwrap();
        let rpc_router = raft_rpc_router(id, CLUSTER_TOKEN, node.rpc_handler()).unwrap();
        servers.push(tokio::spawn(std::future::IntoFuture::into_future(axum::serve(
            listener, rpc_router,
        ))));
        roster.insert(id, peer(&format!("dc{id}"), &format!("http://{addr}/")));
        guards.push(guard);
        nodes.push(node);
    }

    nodes[0].bootstrap(roster).await.unwrap();

    let mut metrics = nodes[0].metrics();
    let elected = tokio::time::timeout(
        Duration::from_secs(10),
        metrics.wait_for(|metrics| metrics.current_leader.is_some()),
    )
    .await
    .unwrap()
    .unwrap()
    .current_leader
    .unwrap();

    let voters = nodes[0].metrics().borrow().membership_config.nodes().count();
    assert_eq!(voters, 3, "all three voters are in the membership");

    let follower = (1..=3u64).find(|id| *id != elected).unwrap();
    let mut follower_metrics = nodes[usize::try_from(follower - 1).unwrap()].metrics();
    tokio::time::timeout(
        Duration::from_secs(10),
        follower_metrics.wait_for(|metrics| metrics.current_leader == Some(elected)),
    )
    .await
    .unwrap()
    .unwrap();
    let home = format!("dc{follower}");
    let group = OwnershipGroup::new(
        nodes[usize::try_from(follower - 1).unwrap()].clone(),
        DatacenterId(home.clone()),
    )
    .with_peer_forwarding(CLUSTER_TOKEN);

    assert_eq!(group.claim_home("proj").await.unwrap().home, home);
    assert_eq!(group.claim_home("proj").await.unwrap().epoch, 1);

    for node in &nodes {
        node.raft().shutdown().await.unwrap();
    }
    for server in servers {
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }
    drop(guards);
}

#[tokio::test]
async fn test_the_rpc_handler_serves_install_snapshot_and_rejects_a_malformed_body() {
    let dir = tempfile::tempdir().unwrap();
    let node = start_node(&dir, RaftConfig::default()).await.unwrap();
    let handler = node.rpc_handler();

    assert!(matches!(
        handler
            .handle(RaftRpc::AppendEntries, Bytes::from_static(b"not a request"))
            .await,
        Err(RaftRpcRejection::Malformed)
    ));

    let request = InstallSnapshotRequest::<TypeConfig> {
        vote: Vote::default(),
        meta: SnapshotMeta::default(),
        offset: 0,
        data: Vec::new(),
        done: true,
    };
    let body = Bytes::from(serde_json::to_vec(&request).unwrap());
    let reply = handler.handle(RaftRpc::InstallSnapshot, body).await.unwrap();

    assert!(!reply.is_empty());
}
