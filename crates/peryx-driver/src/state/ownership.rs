pub use peryx_ha::{ClusterStatus, HomeClaim, OwnershipAuthority, OwnershipError, TransferOutcome};

/// Resolve an authority's committed home, assigning this datacenter when it is unowned.
///
/// # Errors
/// Returns the running group's linearizable resolution or claim error.
pub(super) async fn claim_first_publish_home(
    group: Option<&std::sync::Arc<dyn OwnershipAuthority>>,
    authority: &str,
) -> Result<Option<HomeClaim>, OwnershipError> {
    match group {
        Some(group) => group.claim_home(authority).await.map(Some),
        None => Ok(None),
    }
}

/// Return the committed epoch, or `0` without distributed ownership.
pub(super) async fn committed_authority_epoch(
    group: Option<&std::sync::Arc<dyn OwnershipAuthority>>,
    authority: &str,
) -> u64 {
    match group {
        Some(group) => group.committed_epoch(authority).await,
        None => 0,
    }
}

/// Admit every epoch without distributed ownership; otherwise enforce the committed fence.
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
