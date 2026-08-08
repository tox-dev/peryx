//! The ownership consensus group as the mutation path sees it.
//!
//! Authoritative first-publish home assignment runs through a Raft group, but this neutral crate carries
//! no consensus dependency. The binary implements [`OwnershipAuthority`] over the concrete node and
//! registers it on the [`ServingState`](crate::state::ServingState); a process running no group registers
//! nothing and the mutation path skips the claim.

pub use peryx_ha::{ClusterStatus, HomeClaim, OwnershipAuthority, OwnershipError, TransferOutcome};

/// Claim `authority`'s home on its first publish, best effort, when this process runs a group.
///
/// Skips the claim when no group runs or the authority is already homed, so the common repeat-publish
/// case costs one local read and no consensus round. A claim that cannot commit is logged, never
/// surfaced, so a publish is not blocked on consensus reachability; a node that is not the leader logs
/// and leaves the home to a leader-side claim.
pub(super) async fn claim_first_publish_home(group: Option<&std::sync::Arc<dyn OwnershipAuthority>>, authority: &str) {
    let Some(group) = group else { return };
    if group.has_home(authority).await {
        return;
    }
    if let Err(error) = group.claim_home(authority).await {
        tracing::warn!(%error, authority, "first-publish home claim did not commit");
    }
}

/// The committed authority epoch for `authority`, or `0` when this process runs no group.
///
/// The fence a writer stamps onto its work. A process with no group holds no epoch, reported as the
/// unassigned `0` sentinel the placement fence reads as closed.
pub(super) async fn committed_authority_epoch(
    group: Option<&std::sync::Arc<dyn OwnershipAuthority>>,
    authority: &str,
) -> u64 {
    match group {
        Some(group) => group.committed_epoch(authority).await,
        None => 0,
    }
}

/// Whether work carrying `presented` under `authority` may still be written against the committed epoch.
///
/// A process with no group has no authority to supersede its work, so it admits everything; a running
/// group fences any epoch below its committed one.
pub(super) async fn admit_authority_epoch(
    group: Option<&std::sync::Arc<dyn OwnershipAuthority>>,
    authority: &str,
    presented: u64,
) -> bool {
    match group {
        Some(group) => group.admit_epoch(authority, presented).await,
        None => true,
    }
}

/// Move `authority`'s home to `new_home` on the control quorum, or report the group is absent.
///
/// A process with no group cannot commit a transfer, so it returns `Ok(None)` (nothing moved); a running
/// group commits the fenced move and returns the [`TransferOutcome`], or the [`OwnershipError`] the
/// commit failed with - a control minority surfaces as [`OwnershipError::NotLeader`].
///
/// # Errors
/// The [`OwnershipError`] a running group's commit failed with.
pub(super) async fn transfer_authority_home(
    group: Option<&std::sync::Arc<dyn OwnershipAuthority>>,
    authority: &str,
    new_home: &str,
) -> Result<Option<TransferOutcome>, OwnershipError> {
    match group {
        Some(group) => group.transfer_home(authority, new_home).await,
        None => Ok(None),
    }
}

#[cfg(test)]
#[path = "../../tests/unit/state/ownership/tests.rs"]
mod tests;
