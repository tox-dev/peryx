//! A manifest publish, tag replacement, or delete is a metadata change under the repository's home
//! authority. A mutation snapshots the repository's committed epoch when it starts, then re-admits that
//! epoch just before it commits: a mutation whose home did not move commits under the epoch it leased,
//! while one whose home transferred mid-flight leased a superseded epoch and is turned away before it
//! changes any tag or manifest. A home that fails over yields the survivor's result, and a stale node's
//! late write never exposes a second one.
//!
//! A process with no ownership group, or a repository no group has homed (a committed epoch of `0`),
//! holds no epoch and is never fenced, so a standalone deployment mutates exactly as it did before.

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

/// Snapshot the repository's committed authority epoch to re-admit before a metadata mutation commits.
/// Keyed by the repository's canonical authority key, the same key its home was assigned under.
pub(in crate::registry) async fn repository_epoch(state: &ServingState, repo: &str) -> u64 {
    state.committed_authority_epoch(&crate::name::authority_key(repo)).await
}

/// Whether a mutation leased at `fence` may still commit under `repo`'s committed epoch. A process with
/// no ownership group, or a repository no group has homed (`fence` of `0`), holds no epoch and is never
/// fenced.
pub(in crate::registry) async fn epoch_admits(state: &ServingState, repo: &str, fence: u64) -> bool {
    fence == 0
        || state
            .admit_authority_epoch(&crate::name::authority_key(repo), fence)
            .await
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
