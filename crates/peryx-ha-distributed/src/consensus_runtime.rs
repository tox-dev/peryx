//! Stand up and hold the ownership consensus node an `ha` process runs.
//!
//! A single-datacenter `dc` group and single-node `none` run no consensus, so only [`AvailabilityMode::Ha`]
//! builds anything here. [`ConsensusPlan::from_config`] resolves the roster synchronously - validating
//! every member address and deriving each voter's stable id before any runtime starts - so a bad topology
//! fails config assembly rather than a live node. [`ConsensusPlan::ignite`] then opens the durable log
//! store and assembles the [`RaftNode`], seeding a fresh group with an idempotent bootstrap so a restart
//! rejoins the existing one.
//!
//! [`AvailabilityMode::Ha`]: peryx_ha::AvailabilityMode::Ha

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use std::collections::BTreeSet;

use crate::raft::log_store::RaftLogStoreAdapter;
use crate::raft::network::PeerRaftNetworkFactory;
use crate::raft::{OwnershipResponse, OwnershipStateMachine, PeryxNode, RaftConfig, RaftNode};
use crate::{
    Admission, AssignmentCause, AuthorityEpoch, AuthorityKey, DatacenterId, OwnershipCommand, OwnershipEffect,
    Rejection,
};
use anyhow::{Context as _, bail};
use openraft::error::{ClientWriteError, RaftError};
use openraft::{LogId, StoredMembership};
use peryx_driver::state::{
    ClusterStatus, CommandOutcome, CommandReceipt, ControlCommand, ControlError, HomeClaim, MembershipControl,
    OwnershipAuthority, OwnershipError, TransferOutcome, plan_voter_roster,
};
use peryx_storage::raft::RaftLogStore;
use url::Url;

/// The `u64` voter handle `OpenRaft` keys a node by. Derived from the datacenter identity so every node
/// computes the same id for each voter from the shared roster, without a coordination round or a
/// persisted assignment that could drift from the roster.
type VoterId = u64;

/// How long a single peer Raft RPC may run before it is a retryable loss.
const PEER_RPC_TIMEOUT: Duration = Duration::from_secs(5);

/// The resolved seed for one `ha` node: its own voter id, the roster `OpenRaft` bootstraps from, the
/// durable log path, the group name, and the shared peer credential.
///
/// Built synchronously so every address and roster rule is enforced before a runtime starts; only
/// [`ignite`](Self::ignite) touches the disk or network.
pub struct ConsensusPlan {
    local: VoterId,
    home: DatacenterId,
    /// Whether this node initializes the group. Exactly the writer seeds it; the replicas start empty and
    /// join through the seed's replication, so only one node ever calls `initialize`.
    seed: bool,
    roster: BTreeMap<VoterId, PeryxNode>,
    log_path: PathBuf,
    group: String,
    token: String,
}

impl ConsensusPlan {
    /// Resolve the consensus seed for this process, or `None` when it runs no ownership group.
    ///
    /// A group forms only under `ha` mode with a member roster to name the voters; `none`, `dc`, and an
    /// `ha` process without a roster run no consensus and return `None`. Given a roster, the process must
    /// name itself with a writer identity so it can find its own voter, so an identity that is missing or
    /// absent from the roster is a configuration error caught here.
    ///
    /// # Errors
    /// Returns an error when a rostered `ha` process has no writer identity, when that identity is not a
    /// member, when a member address is not a bare `host:port` authority, when two members collide onto
    /// one voter id, or when the shared peer token cannot be read.
    /// This datacenter's identity, assigned as the home of every authority this node first publishes.
    #[must_use]
    pub fn home(&self) -> DatacenterId {
        self.home.clone()
    }

    /// The shared peer credential the receive-side RPC router authenticates inbound voter RPCs with.
    #[must_use]
    pub fn token(&self) -> &str {
        &self.token
    }

    /// # Errors
    /// Returns an error when the roster cannot form a consensus group.
    pub fn new(
        local_datacenter: String,
        seed: bool,
        members: &[ConsensusMember],
        log_path: PathBuf,
        group: String,
        token: String,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            local: voter_id(&local_datacenter),
            home: DatacenterId(local_datacenter),
            seed,
            roster: build_roster(members)?,
            log_path,
            group,
            token,
        })
    }

    /// Open the durable log store, assemble the node over its three adapters, and bootstrap the group.
    ///
    /// The bootstrap is idempotent, so a restarted node rejoins the group it already formed rather than
    /// failing. Only the seed node forms the group; a node added later joins through an operator-driven
    /// membership change on the existing leader.
    ///
    /// # Errors
    /// Returns an error when the log directory or store cannot be opened, the node cannot start, or the
    /// bootstrap fails for an inconsistent roster.
    pub async fn ignite(&self) -> anyhow::Result<RaftNode> {
        let parent = self.log_path.parent().context("the consensus log path has no parent")?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create the consensus log directory {}", parent.display()))?;
        let store = RaftLogStore::open(&self.log_path)
            .with_context(|| format!("open the consensus log store at {}", self.log_path.display()))?;
        let state_machine = OwnershipStateMachine::with_snapshot_store(store.clone())
            .context("reload the persisted ownership snapshot")?;
        let node = RaftNode::start(
            self.local,
            RaftConfig::default(),
            self.group.clone(),
            PeerRaftNetworkFactory::new(self.token.clone(), PEER_RPC_TIMEOUT),
            RaftLogStoreAdapter::new(store),
            state_machine,
        )
        .await
        .context("start the ownership consensus node")?;
        // Only the writer seeds the group; a replica starts empty and joins through the seed's
        // replication, so exactly one node ever calls initialize and the group cannot split at bootstrap.
        if self.seed {
            node.bootstrap(self.roster.clone())
                .await
                .context("bootstrap the ownership consensus group")?;
        }
        Ok(node)
    }
}

/// The running ownership group as the mutation path reaches it: the node handle plus this datacenter's
/// identity, which every first-publish claim assigns as the authority's home.
pub struct OwnershipGroup {
    node: RaftNode,
    home: DatacenterId,
}

impl OwnershipGroup {
    #[must_use]
    pub const fn new(node: RaftNode, home: DatacenterId) -> Self {
        Self { node, home }
    }
}

#[async_trait::async_trait]
impl OwnershipAuthority for OwnershipGroup {
    async fn has_home(&self, authority: &str) -> bool {
        self.node
            .state_machine()
            .home_of(&AuthorityKey(authority.to_owned()))
            .await
            .is_some()
    }

    async fn committed_epoch(&self, authority: &str) -> u64 {
        self.node
            .state_machine()
            .epoch_of(&AuthorityKey(authority.to_owned()))
            .await
            .0
    }

    async fn admit_epoch(&self, authority: &str, presented: u64) -> bool {
        let admission = self
            .node
            .state_machine()
            .admit(&AuthorityKey(authority.to_owned()), AuthorityEpoch(presented))
            .await;
        matches!(admission, Admission::Admit)
    }

    async fn claim_home(&self, authority: &str) -> Result<HomeClaim, OwnershipError> {
        let command = OwnershipCommand::AssignHome {
            authority: AuthorityKey(authority.to_owned()),
            home: self.home.clone(),
            cause: AssignmentCause::FirstPublish,
        };
        match self.node.submit(command).await {
            // AssignHome commits as either this datacenter winning the home or a rejection because one is
            // already set; both are committed outcomes, and only the first means this call assigned it.
            Ok(response) => Ok(match response {
                OwnershipResponse::Applied(OwnershipEffect::Assigned { .. }) => HomeClaim::AssignedHere,
                _ => HomeClaim::AlreadyHomed,
            }),
            Err(RaftError::APIError(ClientWriteError::ForwardToLeader(forward))) => Err(OwnershipError::NotLeader {
                leader: forward.leader_node.map(|node| node.addr),
            }),
            Err(error) => Err(OwnershipError::Unavailable(error.to_string())),
        }
    }

    async fn transfer_home(&self, authority: &str, new_home: &str) -> Result<Option<TransferOutcome>, OwnershipError> {
        let command = OwnershipCommand::RecordTransfer {
            authority: AuthorityKey(authority.to_owned()),
            new_home: DatacenterId(new_home.to_owned()),
        };
        match self.node.submit(command).await {
            // Only a committed Transferred effect moved the home; a rejection (the authority is unassigned
            // or already homed there) commits too but moves nothing, so it reports no transfer.
            Ok(OwnershipResponse::Applied(OwnershipEffect::Transferred { from, to, epoch })) => {
                Ok(Some(TransferOutcome {
                    from: from.0,
                    to: to.0,
                    epoch: epoch.0,
                }))
            }
            Ok(_) => Ok(None),
            Err(RaftError::APIError(ClientWriteError::ForwardToLeader(forward))) => Err(OwnershipError::NotLeader {
                leader: forward.leader_node.map(|node| node.addr),
            }),
            Err(error) => Err(OwnershipError::Unavailable(error.to_string())),
        }
    }

    fn cluster_status(&self) -> ClusterStatus {
        let metrics = self.node.metrics().borrow().clone();
        let membership = &metrics.membership_config;
        let leader = metrics.current_leader.and_then(|leader| {
            membership
                .nodes()
                .find(|(id, _)| **id == leader)
                .map(|(_, node)| node.datacenter.0.clone())
        });
        ClusterStatus {
            leader,
            term: metrics.current_term,
            voters: membership.nodes().map(|(_, node)| node.datacenter.0.clone()).collect(),
        }
    }
}

#[async_trait::async_trait]
impl MembershipControl for OwnershipGroup {
    async fn submit(&self, command: ControlCommand) -> Result<CommandReceipt, ControlError> {
        match command {
            ControlCommand::AddLearner { datacenter, address } => self.add_learner(&datacenter, address).await,
            ControlCommand::PromoteVoter { datacenter } => self.change_voters(Some(&datacenter), None).await,
            ControlCommand::RemoveVoter { datacenter } => self.change_voters(None, Some(&datacenter)).await,
            ControlCommand::ReplaceVoter {
                remove,
                datacenter,
                address,
            } => {
                self.add_learner(&datacenter, address).await?;
                self.change_voters(Some(&datacenter), Some(&remove)).await
            }
            ControlCommand::TransferAuthority { authority, new_home } => {
                self.submit_ownership(OwnershipCommand::RecordTransfer {
                    authority: AuthorityKey(authority),
                    new_home: DatacenterId(new_home),
                })
                .await
            }
            ControlCommand::AdvanceEpoch { authority } => {
                self.submit_ownership(OwnershipCommand::AdvanceAuthorityEpoch {
                    authority: AuthorityKey(authority),
                })
                .await
            }
        }
    }
}

impl OwnershipGroup {
    /// Add `datacenter` as a non-voting learner reachable at `address`, returning the committed identity
    /// of the membership entry.
    async fn add_learner(&self, datacenter: &str, address: String) -> Result<CommandReceipt, ControlError> {
        let node = PeryxNode {
            datacenter: DatacenterId(datacenter.to_owned()),
            addr: address,
        };
        // A learner does not vote, so the voter roster is unchanged: old and new are the current voters.
        let metrics = self.node.metrics().borrow().clone();
        let voters: BTreeSet<u64> = metrics.membership_config.voter_ids().collect();
        let voters = voter_names(&metrics.membership_config, &voters);
        match self.node.raft().add_learner(voter_id(datacenter), node, false).await {
            Ok(response) => Ok(committed_receipt(
                &response.log_id,
                CommandOutcome::Committed,
                voters.clone(),
                voters,
            )),
            Err(error) => Err(map_write_error(&error)),
        }
    }

    /// Rewrite the voter roster, adding and removing the named datacenters. A rewrite that leaves the
    /// roster unchanged commits as [`CommandOutcome::NoChange`] rather than a distinct entry the operator
    /// did not intend.
    async fn change_voters(&self, add: Option<&str>, remove: Option<&str>) -> Result<CommandReceipt, ControlError> {
        let metrics = self.node.metrics().borrow().clone();
        let current: BTreeSet<u64> = metrics.membership_config.voter_ids().collect();
        let planned = plan_voter_roster(&current, add.map(voter_id), remove.map(voter_id));
        let outcome = if planned == current {
            CommandOutcome::NoChange
        } else {
            CommandOutcome::Committed
        };
        // Name the voter roster the rewrite moves between, so the audit records the transition. The
        // promoted learner already sits in the roster as a node, so its id resolves to a name.
        let old_voters = voter_names(&metrics.membership_config, &current);
        let new_voters = voter_names(&metrics.membership_config, &planned);
        match self.node.raft().change_membership(planned, false).await {
            Ok(response) => Ok(committed_receipt(&response.log_id, outcome, old_voters, new_voters)),
            Err(error) => Err(map_write_error(&error)),
        }
    }

    /// Commit `command` to the ownership log, mapping a rejected transition to an invalid-command error
    /// rather than a committed receipt.
    async fn submit_ownership(&self, command: OwnershipCommand) -> Result<CommandReceipt, ControlError> {
        match self.node.raft().client_write(command).await {
            Ok(response) => match response.data {
                OwnershipResponse::Applied(OwnershipEffect::Rejected(rejection)) => {
                    Err(ControlError::Invalid(rejection_reason(rejection).to_owned()))
                }
                // A transfer or epoch command does not touch the voter roster, so it carries no transition.
                _ => Ok(committed_receipt(
                    &response.log_id,
                    CommandOutcome::Committed,
                    Vec::new(),
                    Vec::new(),
                )),
            },
            Err(error) => Err(map_write_error(&error)),
        }
    }
}

/// The committed identity of a log entry: its term and index, plus the voter roster the command moved the
/// group between, so the audit records the transition. A command that does not touch the roster passes
/// empty sets.
const fn committed_receipt(
    log_id: &LogId<u64>,
    outcome: CommandOutcome,
    old_voters: Vec<String>,
    new_voters: Vec<String>,
) -> CommandReceipt {
    CommandReceipt {
        term: log_id.leader_id.term,
        index: log_id.index,
        outcome,
        old_voters,
        new_voters,
    }
}

/// The datacenter names of the roster's voters named by `voter_ids`, in sorted order, for the audit's
/// roster transition. A learner in the roster is excluded, so the set is the voting membership alone.
fn voter_names(membership: &StoredMembership<u64, PeryxNode>, voter_ids: &BTreeSet<u64>) -> Vec<String> {
    membership
        .nodes()
        .filter(|(id, _)| voter_ids.contains(*id))
        .map(|(_, node)| node.datacenter.0.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Map a client-write failure to a control error, reading the forward target from a not-leader rejection
/// so a caller retries against the leader, and folding every other cause into an unavailable error.
fn map_write_error(error: &RaftError<u64, ClientWriteError<u64, PeryxNode>>) -> ControlError {
    match error {
        RaftError::APIError(ClientWriteError::ForwardToLeader(forward)) => ControlError::NotLeader {
            leader: forward.leader_node.as_ref().map(|node| node.addr.clone()),
        },
        other => ControlError::Unavailable(other.to_string()),
    }
}

/// Why the ownership state machine rejected a transfer or epoch command.
const fn rejection_reason(rejection: Rejection) -> &'static str {
    match rejection {
        Rejection::SameHome => "the authority already homes at that datacenter",
        // A transfer or epoch command reaches only NotAssigned here; AlreadyAssigned arises from a first
        // assignment, never from moving or fencing, so the fallback covers the not-assigned case alone.
        _ => "the authority is not assigned a home to move or fence",
    }
}

/// Map each roster member to its voter id and node data, rejecting a non-authority address or an id
/// collision so the group `OpenRaft` bootstraps from is well-formed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsensusMember {
    pub datacenter: String,
    pub address: String,
}

fn build_roster(members: &[ConsensusMember]) -> anyhow::Result<BTreeMap<VoterId, PeryxNode>> {
    let mut roster = BTreeMap::new();
    for member in members {
        let node = PeryxNode {
            datacenter: DatacenterId(member.datacenter.clone()),
            addr: authority(&member.address)?,
        };
        if let Some(existing) = roster.insert(voter_id(&member.datacenter), node) {
            bail!(
                "datacenter {:?} collides with {:?} on the same consensus voter id",
                member.datacenter,
                existing.datacenter.0
            );
        }
    }
    Ok(roster)
}

/// The bare `host:port` authority a peer's Raft RPCs dial, extracted from the member's advertised URL.
///
/// The network factory prepends its own scheme to this, so a scheme, path, query, or missing port would
/// misdirect or malform the peer URL. Rejecting them here fails a bad roster at config time rather than
/// as a lazy per-peer unreachable error once the node runs.
fn authority(address: &str) -> anyhow::Result<String> {
    let url = Url::parse(address).with_context(|| format!("member address {address:?} is not a valid URL"))?;
    let host = url
        .host_str()
        .with_context(|| format!("member address {address:?} has no host"))?;
    let port = url
        .port()
        .with_context(|| format!("member address {address:?} needs an explicit `host:port`"))?;
    if url.path() != "/" && !url.path().is_empty() {
        bail!("member address {address:?} must be a bare host:port with no path");
    }
    Ok(format!("{host}:{port}"))
}

/// A stable 64-bit id for a datacenter identity, computed with FNV-1a so it is identical on every node
/// and across restarts and toolchains, unlike the standard hasher's unspecified output.
fn voter_id(datacenter: &str) -> VoterId {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for byte in datacenter.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

#[cfg(test)]
#[path = "consensus_runtime_tests.rs"]
mod raft_tests;
