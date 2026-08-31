use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::Duration;

use crate::DatacenterId;
use crate::ownership::{AssignmentCause, OwnershipCommand};
use crate::raft::log_store::RaftLogStoreAdapter;
use crate::raft::network::PeerRaftNetworkFactory;
use crate::raft::persistence::RaftLogStore;
use crate::raft::{OwnershipStateMachine, PeryxNode, RaftConfig, RaftNode};
use openraft::error::{ClientWriteError, ForwardToLeader, RaftError};
use openraft::storage::RaftStateMachine as _;
use openraft::testing::log_id;
use openraft::{Entry, EntryPayload};
use peryx_core::Clock;
use peryx_ha::{
    ClusterStatus, CommandOutcome, ControlCommand, ControlError, HomeClaim, MembershipControl as _,
    OwnershipAuthority as _, OwnershipError, SingletonAcquisition, SingletonLease, SingletonRelease, SingletonRenewal,
    TransferOutcome,
};
use rstest::rstest;
use tempfile::TempDir;

use super::consensus_runtime::{
    ConsensusMember, ConsensusPlan, OwnershipGroup, OwnershipHandle, RaftExecutor, build_roster, map_write_error,
    report_raft_exit, voter_id,
};

const TOKEN: &str = "group-secret";

fn one_voter(dc: &str, endpoint: &str) -> BTreeMap<u64, PeryxNode> {
    BTreeMap::from([(
        voter_id(dc),
        PeryxNode {
            datacenter: DatacenterId(dc.to_owned()),
            endpoint: endpoint.to_owned(),
        },
    )])
}

fn plan_at(log_path: PathBuf, local: u64, roster: BTreeMap<u64, PeryxNode>) -> ConsensusPlan {
    ConsensusPlan {
        local,
        home: DatacenterId("east".to_owned()),
        seed: true,
        roster,
        log_path,
        group: "ownership".to_owned(),
        token: TOKEN.to_owned(),
    }
}

fn blocked_executor() -> (RaftExecutor, Sender<()>, Receiver<()>) {
    let cancellation = tokio_util::sync::CancellationToken::new();
    let (blocked_sender, blocked_receiver) = mpsc::sync_channel(0);
    let (release_sender, release_receiver) = mpsc::channel();
    let (exited_sender, exited_receiver) = mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("blocked-raft-executor".to_owned())
        .spawn(move || {
            blocked_sender.send(()).unwrap();
            release_receiver.recv().unwrap();
            exited_sender.send(()).unwrap();
            anyhow::Ok(())
        })
        .unwrap();
    blocked_receiver.recv().unwrap();
    (RaftExecutor::new(cancellation, thread), release_sender, exited_receiver)
}

#[test]
fn test_dropping_an_executor_returns_while_its_thread_is_blocked() {
    let (executor, release, exited) = blocked_executor();
    let (returned_sender, returned_receiver) = mpsc::channel();
    let caller = std::thread::spawn(move || {
        drop(executor);
        returned_sender.send(()).unwrap();
    });

    returned_receiver.recv().unwrap();
    assert_eq!(exited.try_recv(), Err(TryRecvError::Empty));

    release.send(()).unwrap();
    exited.recv().unwrap();
    caller.join().unwrap();
}

#[test]
fn test_shutting_down_an_executor_returns_while_its_thread_is_blocked() {
    let (executor, release, exited) = blocked_executor();
    let (returned_sender, returned_receiver) = mpsc::channel();
    let caller = std::thread::spawn(move || {
        executor.shutdown();
        returned_sender.send(()).unwrap();
    });

    returned_receiver.recv().unwrap();
    assert_eq!(exited.try_recv(), Err(TryRecvError::Empty));

    release.send(()).unwrap();
    exited.recv().unwrap();
    caller.join().unwrap();
}

fn member(datacenter: &str, address: &str) -> ConsensusMember {
    ConsensusMember {
        datacenter: datacenter.to_owned(),
        address: address.to_owned(),
    }
}

#[rstest]
#[case::plain("http://host.internal:4460", "http://host.internal:4460/")]
#[case::tls("https://host.internal:8443/", "https://host.internal:8443/")]
fn test_build_roster_keeps_the_configured_scheme(#[case] address: &str, #[case] expected: &str) {
    let roster = build_roster(&[member("east", address)]).unwrap();

    assert_eq!(roster[&voter_id("east")].endpoint, expected);
}

#[rstest]
#[case::not_url("not a url", "is not a valid URL")]
#[case::not_http("unix:/var/run/peryx.sock", "http or https scheme")]
#[case::missing_port("http://host.internal", "explicit `host:port`")]
#[case::path("http://host.internal:4460/raft", "no path, query, fragment, or credentials")]
fn test_build_roster_rejects_an_address_the_transport_cannot_dial(#[case] address: &str, #[case] message: &str) {
    let error = build_roster(&[member("east", address)]).unwrap_err().to_string();

    assert!(error.contains(message), "{error}");
}

#[test]
fn test_build_roster_rejects_two_spellings_of_one_endpoint() {
    let members = [
        member("east", "https://peer.internal:443"),
        member("west", "https://PEER.internal:443/"),
    ];

    let error = build_roster(&members).unwrap_err().to_string();

    assert!(error.contains("resolve to the same consensus endpoint"), "{error}");
}

#[test]
fn test_build_roster_rejects_a_voter_id_collision() {
    let members = [
        member("east", "http://a.internal:4460"),
        member("east", "http://b.internal:4460"),
    ];

    let error = build_roster(&members).err().unwrap().to_string();

    assert!(error.contains("same consensus voter id"), "{error}");
}

#[test]
fn test_voter_id_is_stable_and_distinct() {
    assert_eq!(voter_id("east"), voter_id("east"));
    assert_ne!(voter_id("east"), voter_id("west"));
}

#[tokio::test]
async fn test_ignite_does_not_bootstrap_a_replica_seed() {
    let dir = tempfile::tempdir().unwrap();
    let plan = ConsensusPlan {
        local: voter_id("west"),
        home: DatacenterId("west".to_owned()),
        seed: false,
        roster: one_voter("east", "http://east.internal:4460/"),
        log_path: dir.path().join("raft/ownership-log.redb"),
        group: "ownership".to_owned(),
        token: TOKEN.to_owned(),
    };

    let node = plan.ignite().await.unwrap();

    assert_eq!(node.leader(), None);
    node.raft().shutdown().await.unwrap();
}

#[tokio::test]
async fn test_ignite_reports_a_bootstrap_failure() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("raft/ownership-log.redb");
    let error = plan_at(path, voter_id("west"), one_voter("east", "http://east.internal:4460/"))
        .ignite()
        .await
        .err()
        .expect("bootstrap fails");

    assert!(error.to_string().contains("bootstrap"), "{error}");
}

#[tokio::test]
async fn test_ignite_starts_and_bootstraps_a_single_node_group() {
    let dir = tempfile::tempdir().unwrap();
    let plan = ConsensusPlan::new(
        "east".to_owned(),
        true,
        &[ConsensusMember {
            datacenter: "east".to_owned(),
            address: "http://east.internal:4460".to_owned(),
        }],
        dir.path().join("raft/ownership-log.redb"),
        "ownership".to_owned(),
        TOKEN.to_owned(),
    )
    .unwrap();

    assert_eq!(plan.home(), DatacenterId("east".to_owned()));
    assert_eq!(plan.token(), TOKEN);
    assert_eq!(plan.local_voter(), voter_id("east"));

    let node = plan.ignite().await.unwrap();

    let mut metrics = node.metrics();
    tokio::time::timeout(
        Duration::from_secs(5),
        metrics.wait_for(|metrics| metrics.current_leader.is_some()),
    )
    .await
    .unwrap()
    .unwrap();

    assert_eq!(
        node.leader().map(|(_, node)| node.datacenter.0),
        Some("east".to_owned())
    );
    assert!(dir.path().join("raft/ownership-log.redb").exists());
}

#[tokio::test]
async fn test_ignite_fails_when_the_log_directory_cannot_be_created() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path()).unwrap();
    std::fs::write(dir.path().join("raft"), b"not a directory").unwrap();
    let plan = plan_at(
        dir.path().join("raft/ownership-log.redb"),
        voter_id("east"),
        one_voter("east", "http://east.internal:4460/"),
    );

    let error = plan.ignite().await.err().unwrap().to_string();

    assert!(error.contains("log directory"), "{error}");
}

#[tokio::test]
async fn test_ignite_fails_when_the_log_store_cannot_open() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("raft/ownership-log.redb")).unwrap();
    let plan = plan_at(
        dir.path().join("raft/ownership-log.redb"),
        voter_id("east"),
        one_voter("east", "http://east.internal:4460/"),
    );

    let error = plan.ignite().await.err().unwrap().to_string();

    assert!(error.contains("log store"), "{error}");
}

#[tokio::test]
async fn test_ignite_fails_to_start_on_a_corrupt_store() {
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("raft/ownership-log.redb");
    std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();
    RaftLogStore::open(&log_path)
        .unwrap()
        .save_vote(b"not valid json")
        .unwrap();
    let plan = plan_at(
        log_path,
        voter_id("east"),
        one_voter("east", "http://east.internal:4460/"),
    );

    let error = plan.ignite().await.err().unwrap().to_string();

    assert!(error.contains("start the ownership consensus node"), "{error}");
}

#[rstest]
#[case::stopped(Ok(()), "ownership consensus executor stopped unexpectedly")]
#[case::failed(
    Err(anyhow::anyhow!("raft failed")),
    "ownership consensus executor failed: raft failed"
)]
#[tokio::test]
async fn consensus_exit_reaches_shared_supervision(#[case] result: anyhow::Result<()>, #[case] expected: &str) {
    let (lifecycle, mut failures) = crate::lifecycle::Lifecycle::new();
    let cancellation = tokio_util::sync::CancellationToken::new();

    report_raft_exit(&lifecycle, &cancellation, &result);

    assert_eq!(failures.wait().await, expected);
}

#[tokio::test]
async fn test_ignite_fails_to_bootstrap_a_roster_without_the_local_node() {
    let dir = tempfile::tempdir().unwrap();
    let plan = plan_at(
        dir.path().join("raft/ownership-log.redb"),
        voter_id("east"),
        one_voter("west", "http://west.internal:4460/"),
    );

    let error = plan.ignite().await.err().unwrap().to_string();

    assert!(error.contains("bootstrap the ownership consensus group"), "{error}");
}

async fn started_node(dir: &TempDir) -> RaftNode {
    let store = RaftLogStore::open(dir.path().join("raft.redb")).unwrap();
    RaftNode::start(
        voter_id("east"),
        RaftConfig::default(),
        "ownership",
        PeerRaftNetworkFactory::new(TOKEN, Duration::from_secs(1)),
        RaftLogStoreAdapter::new(store),
        OwnershipStateMachine::default(),
    )
    .await
    .unwrap()
}

async fn leader_node(dir: &TempDir) -> RaftNode {
    let node = started_node(dir).await;
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

fn east_claim(epoch: u64) -> HomeClaim {
    HomeClaim {
        home: "east".to_owned(),
        epoch,
    }
}

const JOB: &str = "reclamation";

/// The grant a single-node group commits under its first leader term.
fn granted_lease(holder: &str, generation: u64, now_unix: i64) -> SingletonLease {
    SingletonLease {
        job: JOB.to_owned(),
        holder: holder.to_owned(),
        term: 1,
        generation,
        expires_at_unix: now_unix + peryx_ha::SINGLETON_LEASE_SECS,
    }
}

fn stopped_lease() -> SingletonLease {
    SingletonLease {
        job: JOB.to_owned(),
        holder: "node-a".to_owned(),
        term: 1,
        generation: 1,
        expires_at_unix: 10,
    }
}

fn adjustable_clock(now: i64) -> (Arc<AtomicI64>, Clock) {
    let now = Arc::new(AtomicI64::new(now));
    let source = now.clone();
    (now, Arc::new(move || source.load(Ordering::SeqCst)))
}

#[tokio::test]
async fn test_ownership_handle_delegates_to_a_live_group() {
    let dir = tempfile::tempdir().unwrap();
    let (_, clock) = adjustable_clock(100);
    let group =
        Arc::new(OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned())).with_clock(clock));
    let handle = OwnershipHandle::new(&group);

    assert_eq!(handle.claim_home("proj").await.unwrap(), east_claim(1));
    assert_eq!(handle.committed_epoch("proj").await, 1);
    assert!(handle.admit_epoch("proj", 1).await);
    let lease = handle.begin_epoch_write("proj", 1).await.unwrap().unwrap();
    handle.finish_epoch_write(&lease).await.unwrap();
    assert_eq!(
        handle.transfer_home("proj", "west").await.unwrap(),
        Some(TransferOutcome {
            from: "east".to_owned(),
            to: "west".to_owned(),
            epoch: 2,
        })
    );
    assert_eq!(
        handle
            .submit(
                None,
                ControlCommand::AdvanceEpoch {
                    authority: "proj".to_owned(),
                }
            )
            .await
            .unwrap()
            .receipt
            .outcome,
        CommandOutcome::Committed
    );
    assert_eq!(handle.committed_epoch("proj").await, 3);
    assert_eq!(handle.cluster_status().leader, Some("east".to_owned()));
    let lease = granted_lease("node-a", 1, 100);
    assert_eq!(
        handle.acquire_singleton_lease(JOB, "node-a").await.unwrap(),
        SingletonAcquisition::Acquired(lease.clone())
    );
    assert!(matches!(
        handle.renew_singleton_lease(&lease).await.unwrap(),
        SingletonRenewal::Renewed(_)
    ));
    assert_eq!(
        handle.release_singleton_lease(&lease).await.unwrap(),
        SingletonRelease::Released
    );
}

#[tokio::test]
async fn test_ownership_handle_fails_closed_after_the_group_stops() {
    let dir = tempfile::tempdir().unwrap();
    let group = Arc::new(OwnershipGroup::new(
        leader_node(&dir).await,
        DatacenterId("east".to_owned()),
    ));
    let handle = OwnershipHandle::new(&group);
    drop(group);

    assert_eq!(handle.committed_epoch("proj").await, 0);
    assert!(!handle.admit_epoch("proj", 1).await);
    assert!(matches!(
        handle.begin_epoch_write("proj", 1).await,
        Err(OwnershipError::Unavailable(message)) if message == "ownership consensus stopped"
    ));
    assert!(matches!(
        handle
            .finish_epoch_write(&peryx_ha::AuthorityWriteLease {
                authority: "proj".to_owned(),
                epoch: 1,
                id: "write-1".to_owned(),
                expires_at_unix: 10,
            })
            .await,
        Err(OwnershipError::Unavailable(message)) if message == "ownership consensus stopped"
    ));
    assert!(matches!(
        handle.claim_home("proj").await,
        Err(OwnershipError::Unavailable(message)) if message == "ownership consensus stopped"
    ));
    assert!(matches!(
        handle.transfer_home("proj", "west").await,
        Err(OwnershipError::Unavailable(message)) if message == "ownership consensus stopped"
    ));
    assert!(matches!(
        handle.acquire_singleton_lease(JOB, "node-a").await,
        Err(OwnershipError::Unavailable(message)) if message == "ownership consensus stopped"
    ));
    assert!(matches!(
        handle.renew_singleton_lease(&stopped_lease()).await,
        Err(OwnershipError::Unavailable(message)) if message == "ownership consensus stopped"
    ));
    assert!(matches!(
        handle.release_singleton_lease(&stopped_lease()).await,
        Err(OwnershipError::Unavailable(message)) if message == "ownership consensus stopped"
    ));
    assert_eq!(
        handle
            .submit(
                None,
                ControlCommand::AdvanceEpoch {
                    authority: "proj".to_owned(),
                }
            )
            .await,
        Err(ControlError::Unavailable("ownership consensus stopped".to_owned()))
    );
    assert_eq!(
        handle.cluster_status(),
        ClusterStatus {
            leader: None,
            term: 0,
            voters: Vec::new(),
        }
    );
}

#[tokio::test]
async fn test_claim_home_assigns_then_resolves_the_same_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned()));

    assert_eq!(group.claim_home("proj").await.unwrap(), east_claim(1));
    assert_eq!(group.claim_home("proj").await.unwrap(), east_claim(1));
}

#[tokio::test]
async fn test_claim_home_without_a_leader_reports_not_leader() {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(started_node(&dir).await, DatacenterId("east".to_owned()));

    assert!(matches!(
        group.claim_home("proj").await,
        Err(OwnershipError::NotLeader { leader: None })
    ));
}

#[tokio::test]
async fn test_claim_home_checks_leadership_before_returning_a_cached_assignment() {
    let dir = tempfile::tempdir().unwrap();
    let node = started_node(&dir).await;
    let mut state_machine = node.state_machine().clone();
    state_machine
        .apply([Entry {
            log_id: log_id(1, voter_id("east"), 1),
            payload: EntryPayload::Normal(OwnershipCommand::AssignHome {
                authority: crate::AuthorityKey("proj".to_owned()),
                home: DatacenterId("east".to_owned()),
                cause: AssignmentCause::FirstPublish,
            }),
        }])
        .await
        .unwrap();
    let group = OwnershipGroup::new(node, DatacenterId("east".to_owned()));

    assert!(matches!(
        group.claim_home("proj").await,
        Err(OwnershipError::NotLeader { leader: None })
    ));
}

#[tokio::test]
async fn test_claim_home_on_a_stopped_group_is_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let node = leader_node(&dir).await;
    let group = OwnershipGroup::new(node.clone(), DatacenterId("east".to_owned()));
    assert_eq!(group.claim_home("proj").await.unwrap(), east_claim(1));
    node.raft().shutdown().await.unwrap();

    assert!(matches!(
        group.claim_home("proj").await,
        Err(OwnershipError::Unavailable(_))
    ));
}

#[tokio::test]
async fn test_committed_epoch_reflects_the_first_assignment() {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned()));

    assert_eq!(group.committed_epoch("proj").await, 0);
    assert_eq!(group.claim_home("proj").await.unwrap(), east_claim(1));
    assert_eq!(group.committed_epoch("proj").await, 1);
}

#[tokio::test]
async fn test_admit_epoch_fences_a_superseded_epoch() {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned()));
    assert_eq!(group.claim_home("proj").await.unwrap(), east_claim(1));

    assert!(group.admit_epoch("proj", 1).await, "the committed epoch is admitted");
    assert!(
        !group.admit_epoch("proj", 2).await,
        "an epoch ahead of the committed one is fenced"
    );
    assert!(!group.admit_epoch("proj", 0).await, "the zero sentinel is fenced");
    assert!(
        !group.admit_epoch("other", 1).await,
        "an unassigned authority fences all work"
    );
}

#[tokio::test]
async fn test_write_lease_rejects_unassigned_and_wrong_epoch_releases() {
    let dir = tempfile::tempdir().unwrap();
    let (_, clock) = adjustable_clock(100);
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned())).with_clock(clock);

    assert_eq!(group.begin_epoch_write("ghost", 1).await.unwrap(), None);
    assert_eq!(group.claim_home("proj").await.unwrap(), east_claim(1));
    let lease = group.begin_epoch_write("proj", 1).await.unwrap().unwrap();
    let mut wrong = lease.clone();
    wrong.epoch = 2;
    assert!(matches!(
        group.finish_epoch_write(&wrong).await,
        Err(OwnershipError::Unavailable(_))
    ));
    group.finish_epoch_write(&lease).await.unwrap();
}

#[tokio::test]
async fn test_transfer_before_a_paused_writer_leases_fences_its_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let (_, clock) = adjustable_clock(100);
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned())).with_clock(clock);
    assert_eq!(group.claim_home("proj").await.unwrap(), east_claim(1));
    let snapshot = group.committed_epoch("proj").await;

    assert!(group.transfer_home("proj", "west").await.unwrap().is_some());
    assert_eq!(group.begin_epoch_write("proj", snapshot).await.unwrap(), None);
}

#[tokio::test]
async fn test_transfer_after_a_writer_leases_waits_for_its_finish() {
    let dir = tempfile::tempdir().unwrap();
    let (_, clock) = adjustable_clock(100);
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned())).with_clock(clock);
    assert_eq!(group.claim_home("proj").await.unwrap(), east_claim(1));
    let lease = group.begin_epoch_write("proj", 1).await.unwrap().unwrap();

    assert!(matches!(
        group.transfer_home("proj", "west").await,
        Err(OwnershipError::Unavailable(message)) if message == "authority transfer is blocked by a live write lease"
    ));
    group.finish_epoch_write(&lease).await.unwrap();
    assert_eq!(group.transfer_home("proj", "west").await.unwrap().unwrap().epoch, 2);
}

#[tokio::test]
async fn test_crashed_writer_expires_before_transfer_and_new_epoch_admission() {
    let dir = tempfile::tempdir().unwrap();
    let (now, clock) = adjustable_clock(100);
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned())).with_clock(clock);
    assert_eq!(group.claim_home("proj").await.unwrap(), east_claim(1));
    let _abandoned = group.begin_epoch_write("proj", 1).await.unwrap().unwrap();

    now.store(
        100 + peryx_ha::AUTHORITY_WRITE_LEASE_SECS + peryx_ha::AUTHORITY_CLOCK_SKEW_SECS,
        Ordering::SeqCst,
    );
    assert_eq!(group.transfer_home("proj", "west").await.unwrap().unwrap().epoch, 2);
    assert_eq!(group.begin_epoch_write("proj", 1).await.unwrap(), None);
    assert_eq!(group.begin_epoch_write("proj", 2).await.unwrap().unwrap().epoch, 2);
}

#[tokio::test]
async fn test_transfer_waits_for_every_concurrent_writer() {
    let dir = tempfile::tempdir().unwrap();
    let (_, clock) = adjustable_clock(100);
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned())).with_clock(clock);
    assert_eq!(group.claim_home("proj").await.unwrap(), east_claim(1));
    let first = group.begin_epoch_write("proj", 1).await.unwrap().unwrap();
    let second = group.begin_epoch_write("proj", 1).await.unwrap().unwrap();

    group.finish_epoch_write(&first).await.unwrap();
    assert!(matches!(
        group.transfer_home("proj", "west").await,
        Err(OwnershipError::Unavailable(message)) if message == "authority transfer is blocked by a live write lease"
    ));
    group.finish_epoch_write(&second).await.unwrap();
    assert!(group.transfer_home("proj", "west").await.unwrap().is_some());
}

#[tokio::test]
async fn test_control_transfer_and_epoch_advance_reject_a_live_writer() {
    let dir = tempfile::tempdir().unwrap();
    let (_, clock) = adjustable_clock(100);
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned())).with_clock(clock);
    assert_eq!(group.claim_home("proj").await.unwrap(), east_claim(1));
    let lease = group.begin_epoch_write("proj", 1).await.unwrap().unwrap();

    for command in [
        ControlCommand::TransferAuthority {
            authority: "proj".to_owned(),
            new_home: "west".to_owned(),
        },
        ControlCommand::AdvanceEpoch {
            authority: "proj".to_owned(),
        },
        ControlCommand::ForgetAuthority {
            authority: "proj".to_owned(),
        },
    ] {
        assert!(matches!(
            group.submit(None, command).await,
            Err(ControlError::Invalid(_))
        ));
    }
    group.finish_epoch_write(&lease).await.unwrap();
}

#[tokio::test]
async fn test_a_forget_command_drops_the_home_and_a_repeat_reports_no_change() {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned()));
    assert_eq!(group.claim_home("proj").await.unwrap(), east_claim(1));
    let forget = ControlCommand::ForgetAuthority {
        authority: "proj".to_owned(),
    };

    let dropped = group.submit(None, forget.clone()).await.unwrap();

    assert_eq!(dropped.receipt.outcome, CommandOutcome::Committed);
    assert_eq!(group.committed_epoch("proj").await, 0);
    let repeated = group.submit(None, forget).await.unwrap();
    assert_eq!(repeated.receipt.outcome, CommandOutcome::NoChange);
}

#[tokio::test]
async fn test_transfer_home_moves_a_homed_authority_and_advances_the_epoch() {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned()));
    assert_eq!(group.claim_home("proj").await.unwrap(), east_claim(1));

    let moved = group.transfer_home("proj", "west").await.unwrap();
    assert_eq!(
        moved,
        Some(TransferOutcome {
            from: "east".to_owned(),
            to: "west".to_owned(),
            epoch: 2,
        })
    );
    assert_eq!(group.committed_epoch("proj").await, 2);
    assert!(group.admit_epoch("proj", 2).await, "the new epoch is admitted");
    assert!(!group.admit_epoch("proj", 1).await, "the old home's epoch is fenced");
}

#[tokio::test]
async fn test_transfer_home_of_an_unassigned_authority_moves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned()));

    assert_eq!(group.transfer_home("ghost", "west").await.unwrap(), None);
}

#[tokio::test]
async fn test_transfer_home_to_the_current_home_moves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned()));
    assert_eq!(group.claim_home("proj").await.unwrap(), east_claim(1));

    assert_eq!(group.transfer_home("proj", "east").await.unwrap(), None);
    assert_eq!(group.committed_epoch("proj").await, 1);
}

#[tokio::test]
async fn test_transfer_by_a_control_minority_reports_not_leader() {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(started_node(&dir).await, DatacenterId("east".to_owned()));

    assert!(matches!(
        group.transfer_home("proj", "west").await,
        Err(OwnershipError::NotLeader { leader: None })
    ));
}

#[tokio::test]
async fn test_transfer_home_on_a_stopped_group_is_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let node = leader_node(&dir).await;
    node.raft().shutdown().await.unwrap();
    let group = OwnershipGroup::new(node, DatacenterId("east".to_owned()));

    assert!(matches!(
        group.transfer_home("proj", "west").await,
        Err(OwnershipError::Unavailable(_))
    ));
}

#[tokio::test]
async fn test_cluster_status_reports_the_leader_and_voter_membership() {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned()));

    let status = group.cluster_status();

    assert_eq!(status.leader, Some("east".to_owned()));
    assert!(status.term >= 1, "an elected leader holds a nonzero term");
    assert_eq!(status.voters, vec!["east".to_owned()]);
}

#[tokio::test]
async fn test_cluster_status_excludes_a_committed_learner() {
    let dir = tempfile::tempdir().unwrap();
    let node = leader_node(&dir).await;
    let group = OwnershipGroup::new(node.clone(), DatacenterId("east".to_owned()));

    group.submit(None, add_learner("west")).await.unwrap();

    assert_eq!(
        (
            node.metrics()
                .borrow()
                .membership_config
                .nodes()
                .map(|(_, member)| member.datacenter.0.clone())
                .collect::<BTreeSet<_>>(),
            group.cluster_status().voters,
        ),
        (
            BTreeSet::from(["east".to_owned(), "west".to_owned()]),
            vec!["east".to_owned()],
        )
    );
}

#[tokio::test]
async fn test_membership_publication_times_out() {
    let dir = tempfile::tempdir().unwrap();
    let node = leader_node(&dir).await;
    let (sender, metrics) = tokio::sync::watch::channel(node.metrics().borrow().clone());

    let error = crate::consensus_runtime::wait_for_membership_publication(metrics, 2, Duration::ZERO)
        .await
        .unwrap_err();

    assert_eq!(
        error.to_string(),
        "time out waiting for ownership consensus membership publication"
    );
    drop(sender);
    node.raft().shutdown().await.unwrap();
}

#[tokio::test]
async fn test_membership_publication_fails_when_metrics_close() {
    let dir = tempfile::tempdir().unwrap();
    let node = leader_node(&dir).await;
    let (sender, metrics) = tokio::sync::watch::channel(node.metrics().borrow().clone());
    drop(sender);

    let error = crate::consensus_runtime::wait_for_membership_publication(metrics, 2, Duration::from_secs(5))
        .await
        .unwrap_err();

    assert_eq!(
        error.to_string(),
        "ownership consensus metrics closed before publishing bootstrap membership"
    );
    node.raft().shutdown().await.unwrap();
}

fn add_learner(datacenter: &str) -> ControlCommand {
    ControlCommand::AddLearner {
        datacenter: datacenter.to_owned(),
        address: format!("http://{datacenter}.internal:4470"),
    }
}

#[tokio::test]
async fn test_add_learner_commits_on_the_leader() {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned()));

    let receipt = group.submit(None, add_learner("west")).await.unwrap().receipt;

    assert_eq!(receipt.outcome, CommandOutcome::Committed);
    assert!(
        receipt.term >= 1 && receipt.index >= 1,
        "a committed entry carries a real log id"
    );
}

#[rstest]
#[case::missing_port("west.internal:4470")]
#[case::not_url("not a url")]
#[case::path("http://west.internal:4470/raft")]
#[tokio::test]
async fn test_add_learner_rejects_an_address_static_membership_would_refuse(#[case] address: &str) {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned()));

    let error = group
        .submit(
            None,
            ControlCommand::AddLearner {
                datacenter: "west".to_owned(),
                address: address.to_owned(),
            },
        )
        .await
        .unwrap_err();

    assert!(
        matches!(error, ControlError::Invalid(reason) if reason.contains(address)),
        "{address}"
    );
}

#[tokio::test]
async fn test_add_learner_records_the_canonical_endpoint() {
    let dir = tempfile::tempdir().unwrap();
    let node = leader_node(&dir).await;
    let group = OwnershipGroup::new(node.clone(), DatacenterId("east".to_owned()));

    group
        .submit(
            None,
            ControlCommand::AddLearner {
                datacenter: "west".to_owned(),
                address: "https://WEST.internal:443".to_owned(),
            },
        )
        .await
        .unwrap();

    let learner = node
        .metrics()
        .borrow()
        .membership_config
        .nodes()
        .find_map(|(id, node)| (*id == voter_id("west")).then(|| node.endpoint.clone()));
    assert_eq!(learner.as_deref(), Some("https://west.internal:443/"));
}

#[tokio::test]
async fn test_repeating_a_learner_add_at_its_committed_endpoint_stays_committed() {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned()));
    group.submit(None, add_learner("west")).await.unwrap();

    let repeated = group.submit(None, add_learner("west")).await.unwrap();

    assert!(!repeated.replayed, "an unkeyed retry runs the add again");
    assert_eq!(repeated.receipt.outcome, CommandOutcome::Committed);
}

/// `OpenRaft` keeps the committed node entry, so an unchecked add would report a commit that changed
/// nothing about where the group dials that voter.
#[tokio::test]
async fn test_add_learner_rejects_a_datacenter_whose_voter_id_is_committed_elsewhere() {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned()));

    let error = group
        .submit(
            None,
            ControlCommand::AddLearner {
                datacenter: "east".to_owned(),
                address: "http://east-rebuild.internal:4470".to_owned(),
            },
        )
        .await
        .unwrap_err();

    assert!(
        matches!(&error, ControlError::Invalid(reason)
            if reason.contains("already holds at http://east.internal:4460/")),
        "{error}"
    );
}

#[tokio::test]
async fn test_add_learner_rejects_an_endpoint_another_member_owns() {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned()));

    let error = group
        .submit(
            None,
            ControlCommand::AddLearner {
                datacenter: "west".to_owned(),
                address: "http://east.internal:4460".to_owned(),
            },
        )
        .await
        .unwrap_err();

    assert!(
        matches!(&error, ControlError::Invalid(reason)
            if reason.contains("already belongs to datacenter \"east\"")),
        "{error}"
    );
}

#[tokio::test]
async fn test_replacing_a_voter_at_a_live_member_endpoint_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned()));

    let error = group
        .submit(
            None,
            ControlCommand::ReplaceVoter {
                remove: "east".to_owned(),
                datacenter: "west".to_owned(),
                address: "http://east.internal:4460".to_owned(),
            },
        )
        .await
        .unwrap_err();

    assert!(matches!(error, ControlError::Invalid(_)), "{error}");
}

#[tokio::test]
async fn test_a_membership_command_without_a_leader_reports_the_forward_target() {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(started_node(&dir).await, DatacenterId("east".to_owned()));

    assert!(matches!(
        group.submit(None, add_learner("west")).await,
        Err(ControlError::NotLeader { leader: None })
    ));
}

#[tokio::test]
async fn test_a_command_on_a_stopped_group_is_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let node = leader_node(&dir).await;
    node.raft().shutdown().await.unwrap();
    let group = OwnershipGroup::new(node, DatacenterId("east".to_owned()));

    assert!(matches!(
        group.submit(None, add_learner("west")).await,
        Err(ControlError::Unavailable(_))
    ));
}

#[tokio::test]
async fn test_an_authority_command_on_a_stopped_group_is_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let node = leader_node(&dir).await;
    node.raft().shutdown().await.unwrap();
    let group = OwnershipGroup::new(node, DatacenterId("east".to_owned()));

    assert!(matches!(
        group
            .submit(
                None,
                ControlCommand::AdvanceEpoch {
                    authority: "proj".to_owned(),
                }
            )
            .await,
        Err(ControlError::Unavailable(_))
    ));
}

#[tokio::test]
async fn test_promoting_a_current_voter_is_a_no_op() {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned()));

    let receipt = group
        .submit(
            None,
            ControlCommand::PromoteVoter {
                datacenter: "east".to_owned(),
            },
        )
        .await
        .unwrap()
        .receipt;

    assert_eq!(receipt.outcome, CommandOutcome::NoChange);
}

#[tokio::test]
async fn test_a_membership_receipt_names_the_voter_roster() {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned()));

    // A real promotion needs a second voter; the control layer covers that transition.
    let added = group
        .submit(
            None,
            ControlCommand::AddLearner {
                datacenter: "west".to_owned(),
                address: "http://west.internal:4470".to_owned(),
            },
        )
        .await
        .unwrap()
        .receipt;
    assert_eq!(added.old_voters, ["east"]);
    assert_eq!(added.new_voters, ["east"]);

    let promoted = group
        .submit(
            None,
            ControlCommand::PromoteVoter {
                datacenter: "east".to_owned(),
            },
        )
        .await
        .unwrap()
        .receipt;
    assert_eq!(promoted.outcome, CommandOutcome::NoChange);
    assert_eq!(promoted.old_voters, ["east"]);
    assert_eq!(promoted.new_voters, ["east"]);
}

#[tokio::test]
async fn test_a_roster_rewrite_of_an_unknown_learner_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned()));

    assert!(matches!(
        group
            .submit(
                None,
                ControlCommand::PromoteVoter {
                    datacenter: "west".to_owned(),
                }
            )
            .await,
        Err(ControlError::Unavailable(_))
    ));
}

#[tokio::test]
async fn test_removing_an_absent_voter_is_a_no_op() {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned()));

    let receipt = group
        .submit(
            None,
            ControlCommand::RemoveVoter {
                datacenter: "west".to_owned(),
            },
        )
        .await
        .unwrap()
        .receipt;

    assert_eq!(receipt.outcome, CommandOutcome::NoChange);
}

#[tokio::test]
async fn test_replacing_a_voter_adds_the_learner_then_rewrites_the_roster() {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned()));

    let receipt = group
        .submit(
            None,
            ControlCommand::ReplaceVoter {
                remove: "west".to_owned(),
                datacenter: "west".to_owned(),
                address: "http://west.internal:4470".to_owned(),
            },
        )
        .await
        .unwrap()
        .receipt;

    assert_eq!(receipt.outcome, CommandOutcome::NoChange);
}

/// Serves the voter ID the roster derives, so a peer that answers for another ID is a misdirection.
async fn mounted_node(
    dir: &TempDir,
    datacenter: &str,
) -> (RaftNode, String, tokio::task::JoinHandle<std::io::Result<()>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let id = voter_id(datacenter);
    let store = RaftLogStore::open(dir.path().join("raft.redb")).unwrap();
    let node = RaftNode::start(
        id,
        RaftConfig::default(),
        "ownership",
        PeerRaftNetworkFactory::new(TOKEN, Duration::from_secs(1)),
        RaftLogStoreAdapter::new(store),
        OwnershipStateMachine::default(),
    )
    .await
    .unwrap();
    let router = crate::raft::network::raft_rpc_router(id, TOKEN, node.rpc_handler()).unwrap();
    let served = tokio::spawn(std::future::IntoFuture::into_future(axum::serve(listener, router)));
    (node, format!("http://{address}/"), served)
}

/// Reusing an address is legitimate once its owner is gone, so the endpoint rule must release with the
/// removal rather than fence the address forever.
#[tokio::test]
async fn test_removing_a_voter_frees_its_endpoint_for_the_replacement() {
    let east_dir = tempfile::tempdir().unwrap();
    let west_dir = tempfile::tempdir().unwrap();
    let (east, east_endpoint, east_served) = mounted_node(&east_dir, "east").await;
    let (west, west_endpoint, west_served) = mounted_node(&west_dir, "west").await;
    east.bootstrap(one_voter("east", &east_endpoint)).await.unwrap();
    let mut membership = east.metrics();
    tokio::time::timeout(
        Duration::from_secs(5),
        membership.wait_for(|metrics| metrics.current_leader == Some(voter_id("east"))),
    )
    .await
    .unwrap()
    .unwrap();
    let group = OwnershipGroup::new(east.clone(), DatacenterId("east".to_owned()));
    let joined = ControlCommand::AddLearner {
        datacenter: "west".to_owned(),
        address: west_endpoint.clone(),
    };
    let reused = ControlCommand::AddLearner {
        datacenter: "north".to_owned(),
        address: west_endpoint,
    };
    group.submit(None, joined).await.unwrap();
    group
        .submit(
            None,
            ControlCommand::PromoteVoter {
                datacenter: "west".to_owned(),
            },
        )
        .await
        .unwrap();

    let taken = group.submit(None, reused.clone()).await.unwrap_err();
    group
        .submit(
            None,
            ControlCommand::RemoveVoter {
                datacenter: "west".to_owned(),
            },
        )
        .await
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(5),
        membership.wait_for(|metrics| !metrics.membership_config.nodes().any(|(id, _)| *id == voter_id("west"))),
    )
    .await
    .unwrap()
    .unwrap();
    let freed = group.submit(None, reused).await.unwrap();

    assert!(matches!(taken, ControlError::Invalid(_)), "{taken}");
    assert_eq!(freed.receipt.outcome, CommandOutcome::Committed);
    east.raft().shutdown().await.unwrap();
    west.raft().shutdown().await.unwrap();
    for served in [east_served, west_served] {
        served.abort();
        assert!(served.await.unwrap_err().is_cancelled());
    }
}

#[tokio::test]
async fn test_replacing_a_voter_without_a_leader_forwards_from_the_learner_add() {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(started_node(&dir).await, DatacenterId("east".to_owned()));

    assert!(matches!(
        group
            .submit(
                None,
                ControlCommand::ReplaceVoter {
                    remove: "east".to_owned(),
                    datacenter: "west".to_owned(),
                    address: "http://west.internal:4470".to_owned(),
                }
            )
            .await,
        Err(ControlError::NotLeader { .. })
    ));
}

#[tokio::test]
async fn test_transferring_an_assigned_authority_commits() {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned()));
    assert_eq!(group.claim_home("proj").await.unwrap(), east_claim(1));

    let receipt = group
        .submit(
            None,
            ControlCommand::TransferAuthority {
                authority: "proj".to_owned(),
                new_home: "west".to_owned(),
            },
        )
        .await
        .unwrap()
        .receipt;

    assert_eq!(receipt.outcome, CommandOutcome::Committed);
}

#[tokio::test]
async fn test_repeating_a_transfer_returns_the_committed_no_op() {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned()));
    assert_eq!(group.claim_home("proj").await.unwrap(), east_claim(1));

    let committed = group
        .submit(
            None,
            ControlCommand::TransferAuthority {
                authority: "proj".to_owned(),
                new_home: "west".to_owned(),
            },
        )
        .await
        .unwrap()
        .receipt;
    let repeated = group
        .submit(
            None,
            ControlCommand::TransferAuthority {
                authority: "proj".to_owned(),
                new_home: "west".to_owned(),
            },
        )
        .await
        .unwrap()
        .receipt;

    assert_eq!(
        repeated,
        peryx_ha::CommandReceipt {
            term: committed.term,
            index: committed.index + 1,
            outcome: CommandOutcome::NoChange,
            old_voters: Vec::new(),
            new_voters: Vec::new(),
        }
    );
    assert_eq!(
        group.claim_home("proj").await.unwrap(),
        HomeClaim {
            home: "west".to_owned(),
            epoch: 2,
        }
    );
}

#[tokio::test]
async fn test_transferring_an_unassigned_authority_is_invalid() {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned()));

    let result = group
        .submit(
            None,
            ControlCommand::TransferAuthority {
                authority: "ghost".to_owned(),
                new_home: "west".to_owned(),
            },
        )
        .await;

    assert_eq!(
        result,
        Err(ControlError::Invalid(
            "the authority is not assigned a home to move or fence".to_owned()
        ))
    );
}

#[tokio::test]
async fn test_advancing_an_unassigned_authority_is_invalid() {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned()));

    let result = group
        .submit(
            None,
            ControlCommand::AdvanceEpoch {
                authority: "ghost".to_owned(),
            },
        )
        .await;

    assert!(matches!(result, Err(ControlError::Invalid(_))));
}

#[test]
fn test_a_forward_to_a_known_leader_names_its_address() {
    let error = RaftError::APIError(ClientWriteError::ForwardToLeader(ForwardToLeader {
        leader_id: Some(voter_id("west")),
        leader_node: Some(PeryxNode {
            datacenter: DatacenterId("west".to_owned()),
            endpoint: "http://west.internal:4460/".to_owned(),
        }),
    }));

    assert_eq!(
        map_write_error(&error),
        ControlError::NotLeader {
            leader: Some("http://west.internal:4460/".to_owned()),
        }
    );
}

#[tokio::test]
async fn test_an_authority_command_without_a_leader_reports_the_forward_target() {
    let dir = tempfile::tempdir().unwrap();
    let group =
        OwnershipGroup::new(started_node(&dir).await, DatacenterId("east".to_owned())).with_peer_forwarding(TOKEN);

    assert!(matches!(
        group
            .submit(
                None,
                ControlCommand::AdvanceEpoch {
                    authority: "proj".to_owned(),
                }
            )
            .await,
        Err(ControlError::NotLeader { .. })
    ));
}

#[tokio::test]
async fn test_advancing_an_assigned_authority_commits() {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned()));
    assert_eq!(group.claim_home("proj").await.unwrap(), east_claim(1));

    let receipt = group
        .submit(
            None,
            ControlCommand::AdvanceEpoch {
                authority: "proj".to_owned(),
            },
        )
        .await
        .unwrap()
        .receipt;

    assert_eq!(receipt.outcome, CommandOutcome::Committed);
}

#[tokio::test]
async fn test_one_holder_at_a_time_owns_a_singleton_job() {
    let dir = tempfile::tempdir().unwrap();
    let (_, clock) = adjustable_clock(100);
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned())).with_clock(clock);

    let acquisition = group.acquire_singleton_lease(JOB, "node-a").await.unwrap();

    assert_eq!(
        acquisition,
        SingletonAcquisition::Acquired(granted_lease("node-a", 1, 100))
    );
    assert_eq!(
        group.acquire_singleton_lease(JOB, "node-b").await.unwrap(),
        SingletonAcquisition::Held {
            holder: "node-a".to_owned(),
        }
    );
}

#[tokio::test]
async fn test_renewal_holds_a_singleton_past_its_original_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let (now, clock) = adjustable_clock(100);
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned())).with_clock(clock);
    let lease = granted_lease("node-a", 1, 100);
    group.acquire_singleton_lease(JOB, "node-a").await.unwrap();

    now.store(100 + peryx_ha::SINGLETON_LEASE_SECS - 1, Ordering::SeqCst);
    let renewed = group.renew_singleton_lease(&lease).await.unwrap();
    now.store(
        100 + peryx_ha::SINGLETON_LEASE_SECS + peryx_ha::AUTHORITY_CLOCK_SKEW_SECS,
        Ordering::SeqCst,
    );

    assert!(matches!(renewed, SingletonRenewal::Renewed(_)));
    assert_eq!(
        group.acquire_singleton_lease(JOB, "node-b").await.unwrap(),
        SingletonAcquisition::Held {
            holder: "node-a".to_owned(),
        }
    );
}

#[tokio::test]
async fn test_a_lapsed_singleton_passes_to_the_next_holder() {
    let dir = tempfile::tempdir().unwrap();
    let (now, clock) = adjustable_clock(100);
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned())).with_clock(clock);
    let lease = granted_lease("node-a", 1, 100);
    group.acquire_singleton_lease(JOB, "node-a").await.unwrap();

    let lapsed = 100 + peryx_ha::SINGLETON_LEASE_SECS + peryx_ha::AUTHORITY_CLOCK_SKEW_SECS;
    now.store(lapsed, Ordering::SeqCst);
    let next = group.acquire_singleton_lease(JOB, "node-b").await.unwrap();

    assert_eq!(next, SingletonAcquisition::Acquired(granted_lease("node-b", 2, lapsed)));
    assert_eq!(
        group.renew_singleton_lease(&lease).await.unwrap(),
        SingletonRenewal::Lost
    );
    assert_eq!(
        group.release_singleton_lease(&lease).await.unwrap(),
        SingletonRelease::Lost
    );
}

#[tokio::test]
async fn test_reacquiring_within_one_term_fences_the_previous_holder() {
    let dir = tempfile::tempdir().unwrap();
    let (_, clock) = adjustable_clock(100);
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned())).with_clock(clock);
    let first = granted_lease("node-a", 1, 100);
    group.acquire_singleton_lease(JOB, "node-a").await.unwrap();
    assert_eq!(
        group.release_singleton_lease(&first).await.unwrap(),
        SingletonRelease::Released
    );

    let second = group.acquire_singleton_lease(JOB, "node-b").await.unwrap();

    assert_eq!(second, SingletonAcquisition::Acquired(granted_lease("node-b", 2, 100)));
    assert_eq!(
        group.renew_singleton_lease(&first).await.unwrap(),
        SingletonRenewal::Lost
    );
}
