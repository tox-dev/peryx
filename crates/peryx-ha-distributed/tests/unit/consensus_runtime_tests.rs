use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use crate::DatacenterId;
use crate::raft::log_store::RaftLogStoreAdapter;
use crate::raft::network::PeerRaftNetworkFactory;
use crate::raft::{OwnershipStateMachine, PeryxNode, RaftConfig, RaftNode};
use openraft::error::{ClientWriteError, ForwardToLeader, RaftError};
use peryx_driver::state::{
    CommandOutcome, ControlCommand, ControlError, HomeClaim, MembershipControl as _, OwnershipAuthority as _,
    OwnershipError, TransferOutcome,
};
use peryx_storage::raft::RaftLogStore;
use tempfile::TempDir;

use super::{ConsensusMember, ConsensusPlan, OwnershipGroup, authority, build_roster, map_write_error, voter_id};

const TOKEN: &str = "group-secret";

fn one_voter(dc: &str, addr: &str) -> BTreeMap<u64, PeryxNode> {
    BTreeMap::from([(
        voter_id(dc),
        PeryxNode {
            datacenter: DatacenterId(dc.to_owned()),
            addr: addr.to_owned(),
        },
    )])
}

/// A plan aimed at `log_path`, bypassing `from_config` so a test can drive `ignite`'s failure arms that
/// a validated configuration never produces.
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
#[test]
fn test_authority_extracts_host_and_port() {
    assert_eq!(authority("http://host.internal:4460").unwrap(), "host.internal:4460");
    assert_eq!(authority("https://host.internal:8443/").unwrap(), "host.internal:8443");
}

#[test]
fn test_authority_rejects_a_non_url() {
    assert!(authority("not a url").is_err());
}

#[test]
fn test_authority_rejects_a_missing_host() {
    assert!(authority("unix:/var/run/peryx.sock").is_err());
}

#[test]
fn test_authority_rejects_a_missing_port() {
    let error = authority("http://host.internal").err().unwrap().to_string();
    assert!(error.contains("explicit `host:port`"), "{error}");
}

#[test]
fn test_authority_rejects_a_path() {
    let error = authority("http://host.internal:4460/raft").err().unwrap().to_string();
    assert!(error.contains("bare host:port"), "{error}");
}

#[test]
fn test_build_roster_rejects_a_voter_id_collision() {
    let members = [
        ConsensusMember {
            datacenter: "east".to_owned(),
            address: "http://a.internal:4460".to_owned(),
        },
        ConsensusMember {
            datacenter: "east".to_owned(),
            address: "http://b.internal:4460".to_owned(),
        },
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
    // A replica seed starts but never initializes; it joins through the writer's replication, so it
    // holds no leader until the seed contacts it.
    let plan = ConsensusPlan {
        local: voter_id("west"),
        home: DatacenterId("west".to_owned()),
        seed: false,
        roster: one_voter("east", "east.internal:4460"),
        log_path: dir.path().join("raft/ownership-log.redb"),
        group: "ownership".to_owned(),
        token: TOKEN.to_owned(),
    };

    let node = plan.ignite().await.unwrap();

    assert_eq!(node.leader(), None);
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

    let node = plan.ignite().await.unwrap();

    // The lone voter elects itself within an election window; poll its own leader view rather than
    // reach for openraft's wait helper, which this crate does not depend on directly.
    let mut leader = None;
    for _ in 0..50 {
        if let Some(found) = node.leader() {
            leader = Some(found);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(leader.map(|node| node.datacenter.0), Some("east".to_owned()));
    assert!(dir.path().join("raft/ownership-log.redb").exists());
}

#[tokio::test]
async fn test_ignite_fails_when_the_log_directory_cannot_be_created() {
    let dir = tempfile::tempdir().unwrap();
    // A file where the `raft` directory should be makes the directory creation fail.
    std::fs::create_dir_all(dir.path()).unwrap();
    std::fs::write(dir.path().join("raft"), b"not a directory").unwrap();
    let plan = plan_at(
        dir.path().join("raft/ownership-log.redb"),
        voter_id("east"),
        one_voter("east", "east.internal:4460"),
    );

    let error = plan.ignite().await.err().unwrap().to_string();

    assert!(error.contains("log directory"), "{error}");
}

#[tokio::test]
async fn test_ignite_fails_when_the_log_store_cannot_open() {
    let dir = tempfile::tempdir().unwrap();
    // A directory where the store file should be makes the redb open fail after the parent exists.
    std::fs::create_dir_all(dir.path().join("raft/ownership-log.redb")).unwrap();
    let plan = plan_at(
        dir.path().join("raft/ownership-log.redb"),
        voter_id("east"),
        one_voter("east", "east.internal:4460"),
    );

    let error = plan.ignite().await.err().unwrap().to_string();

    assert!(error.contains("log store"), "{error}");
}

#[tokio::test]
async fn test_ignite_fails_to_start_on_a_corrupt_store() {
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("raft/ownership-log.redb");
    std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();
    // A vote the decoder cannot parse is a fatal storage error the node surfaces while starting.
    RaftLogStore::open(&log_path)
        .unwrap()
        .save_vote(b"not valid json")
        .unwrap();
    let plan = plan_at(log_path, voter_id("east"), one_voter("east", "east.internal:4460"));

    let error = plan.ignite().await.err().unwrap().to_string();

    assert!(error.contains("start the ownership consensus node"), "{error}");
}

#[tokio::test]
async fn test_ignite_fails_to_bootstrap_a_roster_without_the_local_node() {
    let dir = tempfile::tempdir().unwrap();
    // A local id absent from the roster is an inconsistent seed; bootstrap rejects it rather than
    // forming a group the node is not part of.
    let plan = plan_at(
        dir.path().join("raft/ownership-log.redb"),
        voter_id("east"),
        one_voter("west", "west.internal:4460"),
    );

    let error = plan.ignite().await.err().unwrap().to_string();

    assert!(error.contains("bootstrap the ownership consensus group"), "{error}");
}

async fn started_node(dir: &TempDir) -> RaftNode {
    let store = RaftLogStore::open(dir.path().join("raft.redb")).unwrap();
    // Key the node by the same id the production roster derives from the datacenter, so a membership
    // command that maps a datacenter back to its voter id reaches the actual voter.
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
            addr: "east.internal:4460".to_owned(),
        },
    )]))
    .await
    .unwrap();
    for _ in 0..50 {
        if node.leader().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    node
}

#[tokio::test]
async fn test_claim_home_assigns_on_first_publish_then_reports_already_homed() {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned()));

    assert_eq!(group.claim_home("proj").await.unwrap(), HomeClaim::AssignedHere);
    // The home persists in the group, so a repeat publish, or a race another datacenter won, reads as
    // already homed rather than reassigning.
    assert_eq!(group.claim_home("proj").await.unwrap(), HomeClaim::AlreadyHomed);
}

#[tokio::test]
async fn test_claim_home_without_a_leader_reports_not_leader() {
    let dir = tempfile::tempdir().unwrap();
    // An unbootstrapped node has no leader, so the claim cannot commit here and names no forward target.
    let group = OwnershipGroup::new(started_node(&dir).await, DatacenterId("east".to_owned()));

    assert!(matches!(
        group.claim_home("proj").await,
        Err(OwnershipError::NotLeader { leader: None })
    ));
}

#[tokio::test]
async fn test_claim_home_on_a_stopped_group_is_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let node = leader_node(&dir).await;
    node.raft().shutdown().await.unwrap();
    let group = OwnershipGroup::new(node, DatacenterId("east".to_owned()));

    assert!(matches!(
        group.claim_home("proj").await,
        Err(OwnershipError::Unavailable(_))
    ));
}

#[tokio::test]
async fn test_has_home_reflects_a_committed_assignment() {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned()));

    assert!(!group.has_home("proj").await);
    group.claim_home("proj").await.unwrap();
    // client_write returns after the entry applies, so the home reads back locally at once.
    assert!(group.has_home("proj").await);
}

#[tokio::test]
async fn test_committed_epoch_reflects_the_first_assignment() {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned()));

    // Unassigned reads as the zero sentinel; the first committed assignment mints epoch one.
    assert_eq!(group.committed_epoch("proj").await, 0);
    group.claim_home("proj").await.unwrap();
    assert_eq!(group.committed_epoch("proj").await, 1);
}

#[tokio::test]
async fn test_admit_epoch_fences_a_superseded_epoch() {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned()));
    group.claim_home("proj").await.unwrap();

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
async fn test_transfer_home_moves_a_homed_authority_and_advances_the_epoch() {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned()));
    group.claim_home("proj").await.unwrap();

    let moved = group.transfer_home("proj", "west").await.unwrap();
    assert_eq!(
        moved,
        Some(TransferOutcome {
            from: "east".to_owned(),
            to: "west".to_owned(),
            epoch: 2,
        })
    );
    // The transfer mints the next epoch, which fences the old home's stale-epoch writes.
    assert_eq!(group.committed_epoch("proj").await, 2);
    assert!(group.admit_epoch("proj", 2).await, "the new epoch is admitted");
    assert!(!group.admit_epoch("proj", 1).await, "the old home's epoch is fenced");
}

#[tokio::test]
async fn test_transfer_home_of_an_unassigned_authority_moves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned()));

    // An authority with no home has nothing to move; the command commits but reports no transfer.
    assert_eq!(group.transfer_home("ghost", "west").await.unwrap(), None);
}

#[tokio::test]
async fn test_transfer_home_to_the_current_home_moves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned()));
    group.claim_home("proj").await.unwrap();

    // Moving to the home it already holds is a no-op, not a spurious epoch bump.
    assert_eq!(group.transfer_home("proj", "east").await.unwrap(), None);
    assert_eq!(group.committed_epoch("proj").await, 1);
}

#[tokio::test]
async fn test_transfer_by_a_control_minority_reports_not_leader() {
    let dir = tempfile::tempdir().unwrap();
    // An unbootstrapped node holds no leadership, so it cannot commit a transfer: a control minority
    // never moves authority.
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

fn add_learner(datacenter: &str) -> ControlCommand {
    ControlCommand::AddLearner {
        datacenter: datacenter.to_owned(),
        address: format!("{datacenter}.internal:4470"),
    }
}

#[tokio::test]
async fn test_add_learner_commits_on_the_leader() {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned()));

    let receipt = group.submit(add_learner("west")).await.unwrap();

    assert_eq!(receipt.outcome, CommandOutcome::Committed);
    assert!(
        receipt.term >= 1 && receipt.index >= 1,
        "a committed entry carries a real log id"
    );
}

#[tokio::test]
async fn test_a_membership_command_without_a_leader_reports_the_forward_target() {
    let dir = tempfile::tempdir().unwrap();
    // An unbootstrapped node has no leader, so the add cannot commit and names no forward target.
    let group = OwnershipGroup::new(started_node(&dir).await, DatacenterId("east".to_owned()));

    assert!(matches!(
        group.submit(add_learner("west")).await,
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
        group.submit(add_learner("west")).await,
        Err(ControlError::Unavailable(_))
    ));
}

#[tokio::test]
async fn test_promoting_a_current_voter_is_a_no_op() {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned()));

    // East is already the sole voter, so promoting it leaves the roster unchanged and commits no distinct
    // entry.
    let receipt = group
        .submit(ControlCommand::PromoteVoter {
            datacenter: "east".to_owned(),
        })
        .await
        .unwrap();

    assert_eq!(receipt.outcome, CommandOutcome::NoChange);
}

#[tokio::test]
async fn test_a_membership_receipt_names_the_voter_roster() {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned()));

    // A learner does not vote, so the roster stays at the single voter, named on both sides of the
    // receipt: the audit records the voter set the command left the group at. (A committed voter-set
    // transition is asserted at the control layer; a real promotion needs a second live node to ack the
    // new-config quorum, which a single-process test cannot provide.)
    let added = group
        .submit(ControlCommand::AddLearner {
            datacenter: "west".to_owned(),
            address: "west.internal:4470".to_owned(),
        })
        .await
        .unwrap();
    assert_eq!(added.old_voters, ["east"]);
    assert_eq!(added.new_voters, ["east"]);

    // A no-op promotion of the sole voter commits without a new-config quorum, still naming the roster.
    let promoted = group
        .submit(ControlCommand::PromoteVoter {
            datacenter: "east".to_owned(),
        })
        .await
        .unwrap();
    assert_eq!(promoted.outcome, CommandOutcome::NoChange);
    assert_eq!(promoted.old_voters, ["east"]);
    assert_eq!(promoted.new_voters, ["east"]);
}

#[tokio::test]
async fn test_a_roster_rewrite_of_an_unknown_learner_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    // Promoting a datacenter that was never added as a learner is a real roster change the leader refuses,
    // returning at once rather than blocking on a quorum it will never reach.
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned()));

    assert!(matches!(
        group
            .submit(ControlCommand::PromoteVoter {
                datacenter: "west".to_owned(),
            })
            .await,
        Err(ControlError::Unavailable(_))
    ));
}

#[tokio::test]
async fn test_removing_an_absent_voter_is_a_no_op() {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned()));

    // The removed datacenter is not a voter, so the roster is unchanged and the command commits no entry.
    let receipt = group
        .submit(ControlCommand::RemoveVoter {
            datacenter: "west".to_owned(),
        })
        .await
        .unwrap();

    assert_eq!(receipt.outcome, CommandOutcome::NoChange);
}

#[tokio::test]
async fn test_replacing_a_voter_adds_the_learner_then_rewrites_the_roster() {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned()));

    // The incoming datacenter is added as a learner, then the roster rewrite runs; adding and removing the
    // same datacenter leaves the voter set unchanged, so the two-step command commits without waiting on a
    // learner to catch up.
    let receipt = group
        .submit(ControlCommand::ReplaceVoter {
            remove: "west".to_owned(),
            datacenter: "west".to_owned(),
            address: "west.internal:4470".to_owned(),
        })
        .await
        .unwrap();

    assert_eq!(receipt.outcome, CommandOutcome::NoChange);
}

#[tokio::test]
async fn test_replacing_a_voter_without_a_leader_forwards_from_the_learner_add() {
    let dir = tempfile::tempdir().unwrap();
    // The replace adds the incoming learner first; on an unbootstrapped node that add already forwards to
    // a leader, so the command returns before the roster rewrite.
    let group = OwnershipGroup::new(started_node(&dir).await, DatacenterId("east".to_owned()));

    assert!(matches!(
        group
            .submit(ControlCommand::ReplaceVoter {
                remove: "east".to_owned(),
                datacenter: "west".to_owned(),
                address: "west.internal:4470".to_owned(),
            })
            .await,
        Err(ControlError::NotLeader { .. })
    ));
}

#[tokio::test]
async fn test_transferring_an_assigned_authority_commits() {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned()));
    group.claim_home("proj").await.unwrap();

    let receipt = group
        .submit(ControlCommand::TransferAuthority {
            authority: "proj".to_owned(),
            new_home: "west".to_owned(),
        })
        .await
        .unwrap();

    assert_eq!(receipt.outcome, CommandOutcome::Committed);
}

#[tokio::test]
async fn test_transferring_to_the_same_home_is_invalid() {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned()));
    group.claim_home("proj").await.unwrap();

    let result = group
        .submit(ControlCommand::TransferAuthority {
            authority: "proj".to_owned(),
            new_home: "east".to_owned(),
        })
        .await;

    assert!(matches!(result, Err(ControlError::Invalid(_))));
}

#[tokio::test]
async fn test_advancing_an_unassigned_authority_is_invalid() {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned()));

    let result = group
        .submit(ControlCommand::AdvanceEpoch {
            authority: "ghost".to_owned(),
        })
        .await;

    assert!(matches!(result, Err(ControlError::Invalid(_))));
}

#[test]
fn test_a_forward_to_a_known_leader_names_its_address() {
    // A follower's rejection stamps the leader it knows; the control error carries that address so a
    // client retries against it rather than guessing.
    let error = RaftError::APIError(ClientWriteError::ForwardToLeader(ForwardToLeader {
        leader_id: Some(voter_id("west")),
        leader_node: Some(PeryxNode {
            datacenter: DatacenterId("west".to_owned()),
            addr: "west.internal:4460".to_owned(),
        }),
    }));

    assert_eq!(
        map_write_error(&error),
        ControlError::NotLeader {
            leader: Some("west.internal:4460".to_owned()),
        }
    );
}

#[tokio::test]
async fn test_an_authority_command_without_a_leader_reports_the_forward_target() {
    let dir = tempfile::tempdir().unwrap();
    // An unbootstrapped node cannot commit an ownership command, so the client write forwards to a leader
    // it does not know rather than committing here.
    let group = OwnershipGroup::new(started_node(&dir).await, DatacenterId("east".to_owned()));

    assert!(matches!(
        group
            .submit(ControlCommand::AdvanceEpoch {
                authority: "proj".to_owned(),
            })
            .await,
        Err(ControlError::NotLeader { .. })
    ));
}

#[tokio::test]
async fn test_advancing_an_assigned_authority_commits() {
    let dir = tempfile::tempdir().unwrap();
    let group = OwnershipGroup::new(leader_node(&dir).await, DatacenterId("east".to_owned()));
    group.claim_home("proj").await.unwrap();

    let receipt = group
        .submit(ControlCommand::AdvanceEpoch {
            authority: "proj".to_owned(),
        })
        .await
        .unwrap();

    assert_eq!(receipt.outcome, CommandOutcome::Committed);
}
