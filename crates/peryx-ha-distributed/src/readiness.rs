//! Aggregating a datacenter group's configured member frontiers into readiness and a durable frontier.
//!
//! A datacenter group replicates one writer's metadata journal to its replicas. Each member reports the
//! highest serial it has durably applied; [`group_readiness`] folds those observations and the selected
//! [`DurabilityPolicy`] into whether the group can acknowledge a new write and the highest serial it can
//! guarantee is durable. The fold is pure: frontiers are an input, never fetched, so the result is
//! decided without any transport, clock, or storage — the acknowledgement decision that acts on it lives
//! elsewhere.
//!
//! Two invariants shape the result. The writer is the sole source of serials and a replica applies only
//! what the writer has produced, so the writer's applied serial bounds every member's. And membership is
//! static: the snapshot names every configured member, a lost one as [`MemberFrontier::applied`] `None`,
//! so the policy's quorum is measured against a fixed group size and a vanished member cannot silently
//! shrink it into a smaller, unsafe quorum.

pub use peryx_ha::DurabilityPolicy;
use std::cmp::Reverse;
use std::collections::BTreeSet;

use crate::visibility::Frontier as VisibilityFrontier;

/// A member's role in its datacenter group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberRole {
    /// The single node that accepts authoritative writes and issues the journal serials.
    Writer,
    /// A read replica that applies the writer's journal.
    Replica,
}

/// One configured member's contribution to durability at the instant a snapshot is taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberFrontier {
    /// The member's stable identity within the group.
    pub member: String,
    pub role: MemberRole,
    /// The highest metadata serial this member has durably applied, or `None` when it is not currently
    /// reporting a frontier — unreachable or never observed. A non-reporting member holds no serial, so
    /// it never counts toward durability, and a non-reporting writer leaves the group unready.
    pub applied: Option<u64>,
}

/// Why a group cannot acknowledge a new write at its policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessBlocker {
    /// No member with the writer role is reporting a frontier, so no new authoritative write can be
    /// issued or acknowledged until the writer returns.
    WriterLost,
    /// A writer is present, but fewer members are reporting a frontier than the policy requires, so a new
    /// write could not reach durability.
    InsufficientMembers { reporting: usize, required: usize },
}

/// A datacenter group's readiness and durable frontier under a durability policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupReadiness {
    /// `None` when the group can acknowledge a new write at the policy, otherwise why it cannot.
    pub blocked: Option<ReadinessBlocker>,
    /// The highest serial the policy's required number of members have durably applied — the group's
    /// guaranteed-durable frontier. Independent of [`blocked`](Self::blocked): a serial already held by
    /// enough members stays durable even when a lost writer stops new writes from being acknowledged.
    pub durable_frontier: u64,
}

impl GroupReadiness {
    /// Whether the group can acknowledge a new write at its durability policy.
    #[must_use]
    pub const fn is_ready(&self) -> bool {
        self.blocked.is_none()
    }
}

/// Aggregate a datacenter group's member frontiers into its readiness and durable frontier under
/// `policy`.
///
/// `members` is the full configured roster: every member appears once, a lost one carrying
/// [`MemberFrontier::applied`] `None`, so the policy's quorum is measured against the fixed group size.
/// The group is ready when a writer is reporting and at least the policy's required number of members are
/// reporting, so a single lagging or lost replica never blocks readiness while the rest still satisfy the
/// policy. The durable frontier is the highest serial the required number of members have applied — the
/// minimum over the fastest such members, never dragged down to the slowest member overall.
#[must_use]
pub fn group_readiness(members: &[MemberFrontier], policy: DurabilityPolicy) -> GroupReadiness {
    // A member id is unique in the roster: one physical node listed twice is still one member. Deduplicate
    // by id, first occurrence winning, so a doubled node cannot inflate both the roster size and the
    // reporting count into a quorum the real group never reaches. This upholds the call-site invariant the
    // pure fold cannot otherwise enforce.
    let mut seen = BTreeSet::new();
    let roster: Vec<&MemberFrontier> = members
        .iter()
        .filter(|member| seen.insert(member.member.as_str()))
        .collect();

    let required = policy.required_acks(roster.len());
    let reporting = roster.iter().filter(|member| member.applied.is_some()).count();
    let writer = roster
        .iter()
        .find(|member| member.role == MemberRole::Writer && member.applied.is_some());
    let blocked = if writer.is_none() {
        Some(ReadinessBlocker::WriterLost)
    } else if reporting < required {
        Some(ReadinessBlocker::InsufficientMembers { reporting, required })
    } else {
        None
    };
    // The writer bounds every member's applied serial, so the durable frontier can never legitimately
    // exceed the writer's. Clamp to it when the writer reports, so a replica that erroneously advertises a
    // serial above the writer cannot over-claim durability to its frontier under `Local` (or any
    // `required == 1`) path.
    let frontier = durable_frontier(&roster, required);
    let bounded = writer.map_or(frontier, |writer| frontier.min(writer.applied.unwrap_or(0)));
    GroupReadiness {
        blocked,
        durable_frontier: bounded,
    }
}

/// The visibility compaction frontier for a datacenter group at authority `epoch`: the highest serial
/// both the group's required members and the backup have durably applied.
///
/// [`VisibilityState::compact`](crate::VisibilityState::compact) releases a tombstone only up to this
/// frontier, so an operation is forgotten only once no lagging replica or backup restore can still
/// re-litigate it. The backup frontier bounds the replicated frontier: a serial the backup has not yet
/// stored stays retained even after the replicas hold it, because a restore from that backup would
/// otherwise arrive without the operation.
///
/// Authority epochs are contiguous from one and a superseded epoch is fully drained before the next
/// begins, so the frontier marks every epoch below `epoch` covered without bound and the current `epoch`
/// covered only through its durable serial. Without the drained epochs an entry whose high-water sits in
/// a superseded epoch never clears the frontier and is retained forever, even though no operation in that
/// epoch can still arrive.
#[must_use]
pub fn visibility_compaction_frontier(
    members: &[MemberFrontier],
    policy: DurabilityPolicy,
    backup_applied: u64,
    epoch: u64,
) -> VisibilityFrontier {
    let replicated = group_readiness(members, policy).durable_frontier;
    let mut frontier = VisibilityFrontier::default();
    for drained in 1..epoch {
        frontier.acknowledge(drained, u64::MAX);
    }
    frontier.acknowledge(epoch, replicated.min(backup_applied));
    frontier
}

/// The highest serial at least `required` members have applied: the `required`-th largest applied serial,
/// counting a non-reporting member as zero. Zero when the policy needs more acknowledgements than the
/// group holds, since no serial then clears the bar.
fn durable_frontier(members: &[&MemberFrontier], required: usize) -> u64 {
    if required == 0 || required > members.len() {
        return 0;
    }
    let mut applied: Vec<u64> = members.iter().map(|member| member.applied.unwrap_or(0)).collect();
    applied.sort_unstable_by_key(|&serial| Reverse(serial));
    applied[required - 1]
}
