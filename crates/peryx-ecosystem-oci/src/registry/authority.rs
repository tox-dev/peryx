//! A manifest publish, tag replacement, or delete is a metadata change under the repository's home
//! authority. A mutation snapshots the committed epoch, then acquires a bounded quorum lease before its
//! metadata transaction. A transfer cannot commit while that lease is live, and an expired writer cannot
//! commit after a transfer.
//!
//! A process with no ownership group holds no epoch, so a standalone deployment mutates exactly as it
//! did before. A configured group fails closed until the repository has a committed home.

use axum::response::Response;
use peryx_driver::ServingState;
use peryx_storage::meta::QuotaReservationRecord;

use super::ServeError;
use crate::error::{ErrorCode, error_response};

/// Resolve the repository's committed home and return its publication epoch. The canonical key keeps
/// OCI authorities separate from other ecosystems.
pub(in crate::registry) async fn claim_repository_home(state: &ServingState, repo: &str) -> Result<u64, Response> {
    let authority = crate::name::authority_key(repo);
    match state.claim_first_publish_home(&authority).await {
        Ok(None) => Ok(0),
        Ok(Some(claim)) if state.availability_topology().local_datacenter() == Some(claim.home.as_str()) => {
            Ok(claim.epoch)
        }
        Ok(Some(_)) => Err(authority_moved()),
        Err(error) => {
            tracing::warn!(%error, authority, "first-publish home could not be resolved");
            Err(authority_moved())
        }
    }
}

/// Snapshot the repository's committed authority epoch before a metadata mutation.
pub(in crate::registry) async fn repository_epoch(state: &ServingState, repo: &str) -> u64 {
    state.committed_authority_epoch(&crate::name::authority_key(repo)).await
}

pub(in crate::registry) enum EpochCommit<T> {
    Committed(T),
    Fenced,
}

pub(in crate::registry) async fn commit_epoch<T>(
    state: &ServingState,
    repo: &str,
    fence: u64,
    mutation: impl FnOnce(&EpochLease<'_>) -> Result<T, ServeError>,
) -> Result<EpochCommit<T>, ServeError> {
    commit_authority_epoch(state, &crate::name::authority_key(repo), fence, mutation).await
}

pub(in crate::registry) async fn commit_authority_epoch<T>(
    state: &ServingState,
    authority: &str,
    fence: u64,
    mutation: impl FnOnce(&EpochLease<'_>) -> Result<T, ServeError>,
) -> Result<EpochCommit<T>, ServeError> {
    let lease = match state.begin_authority_epoch_write(authority, fence).await {
        Ok(Some(lease)) => Some(lease),
        Ok(None) if state.ownership_authority().is_none() => None,
        Ok(None) | Err(_) => return Ok(EpochCommit::Fenced),
    };
    let lease = EpochLease { state, lease };
    let result = if lease.check() {
        match mutation(&lease) {
            Ok(value) => Ok(EpochCommit::Committed(value)),
            Err(ServeError::Fenced) => Ok(EpochCommit::Fenced),
            Err(error) => Err(error),
        }
    } else {
        Ok(EpochCommit::Fenced)
    };
    lease.finish().await;
    result
}

pub(in crate::registry) struct EpochLease<'a> {
    state: &'a ServingState,
    lease: Option<peryx_ha::AuthorityWriteLease>,
}

impl EpochLease<'_> {
    pub(in crate::registry) fn check(&self) -> bool {
        self.lease
            .as_ref()
            .is_none_or(|lease| lease.admits((self.state.clock)()))
    }

    pub(in crate::registry) fn guard(&self) -> Result<(), ServeError> {
        if self.check() { Ok(()) } else { Err(ServeError::Fenced) }
    }

    async fn finish(self) {
        self.state.release_authority_epoch_write(self.lease).await;
    }
}

/// The retry response for a mutation whose repository authority advanced past the epoch it leased. It is
/// a `503` a client retries, and it names no leader, datacenter, or membership, so it leaks no topology.
pub(in crate::registry) fn authority_moved() -> Response {
    error_response(
        ErrorCode::Unavailable,
        "the repository authority moved while the request was in flight; retry the request",
    )
}

/// Release a still-open quota reservation a fenced push had taken, a no-op when the push was unmetered,
/// so a mutation turned away by the fence leaves no phantom accounting behind.
pub(in crate::registry) fn release_reservation(
    state: &ServingState,
    reservation: Option<QuotaReservationRecord>,
) -> Result<(), ServeError> {
    if let Some(record) = reservation {
        state.meta.release_quota_reservation(record.id)?;
    }
    Ok(())
}
