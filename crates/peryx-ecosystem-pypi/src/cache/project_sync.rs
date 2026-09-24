//! The catalog job's per-project refresh, through the page fetch requests use.
//!
//! The page cache is the one store for a project's files: storing a page registers each file's source,
//! metadata sibling, and provenance. A job refresh therefore takes the page's flight like a request does,
//! so a job and a request for one project make one upstream request.

use peryx_driver::state::ServingState;
use peryx_storage::meta::MetaError;
use peryx_upstream::{UpstreamClient, UpstreamError};

use super::fetch::{PageFetch, fetch_page};
use super::{CacheError, flight_gate, fresh_cached, project_negative_key, release_flight};

/// What one project refresh did to the page peryx serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectSyncOutcome {
    /// This refresh stored a page whose body differs from the one peryx held, or the first one.
    Changed,
    /// The stored page still answers: it was fresh, or upstream confirmed or resent it unchanged.
    Unchanged,
    /// Upstream no longer has the project, so its page is retired.
    Missing,
    /// The repository policy refuses to cache the project, so no request was made.
    Denied,
}

/// A project page could not be refreshed.
#[derive(Debug, thiserror::Error)]
pub enum ProjectSyncError {
    #[error(transparent)]
    Upstream(#[from] UpstreamError),
    #[error("upstream project detail returned {0}")]
    Status(u16),
    /// Upstream answered with a page peryx refuses or cannot attribute to the page it holds, or the local
    /// upstream limit stayed full; the next project is unaffected.
    #[error(transparent)]
    Page(CacheError),
    #[error(transparent)]
    Store(#[from] MetaError),
    /// A local failure no other project would escape.
    #[error(transparent)]
    Internal(CacheError),
}

impl From<CacheError> for ProjectSyncError {
    fn from(error: CacheError) -> Self {
        match error {
            CacheError::Upstream(error) => Self::Upstream(error),
            CacheError::Meta(error) => Self::Store(error),
            error @ (CacheError::Parse(_)
            | CacheError::Simple(_)
            | CacheError::Unavailable
            | CacheError::RateLimited { .. }) => Self::Page(error),
            error => Self::Internal(error),
        }
    }
}

/// Refresh `project`'s page on `index` the way a request would.
///
/// An upstream failure is reported instead of serving the stale page, since the job's report would otherwise
/// count a page upstream never confirmed.
///
/// # Errors
/// Returns [`ProjectSyncError`] when the page cannot be refreshed; the stored page is left unchanged.
pub async fn refresh_project_page(
    state: &ServingState,
    index: &str,
    project: &str,
    client: &UpstreamClient,
) -> Result<ProjectSyncOutcome, ProjectSyncError> {
    let key = format!("{index}/{project}");
    loop {
        // A full upstream limit means requests are using it; wait out its horizon off the page's flight, so a
        // request for this project is not held behind the wait, then try again.
        match refresh_once(state, &key, index, project, client).await {
            Err(ProjectSyncError::Page(CacheError::RateLimited { retry_after })) => {
                tokio::time::sleep(std::time::Duration::from_secs(retry_after.max(1))).await;
            }
            result => return result,
        }
    }
}

async fn refresh_once(
    state: &ServingState,
    key: &str,
    index: &str,
    project: &str,
    client: &UpstreamClient,
) -> Result<ProjectSyncOutcome, ProjectSyncError> {
    let guard = flight_gate(state, key).lock_owned().await;
    let result = refresh_held(state, key, index, project, client).await;
    release_flight(state, key, guard);
    result
}

async fn refresh_held(
    state: &ServingState,
    key: &str,
    index: &str,
    project: &str,
    client: &UpstreamClient,
) -> Result<ProjectSyncOutcome, ProjectSyncError> {
    // A page or negative answer a request stored while this waited stands in for a request of its own.
    if state.negative_fresh(&project_negative_key(key)) {
        return Ok(ProjectSyncOutcome::Missing);
    }
    if fresh_cached(state, key)?.is_some() {
        return Ok(ProjectSyncOutcome::Unchanged);
    }
    let fetched = match fetch_page(state, key, index, project, client).await {
        Err(CacheError::Policy(_)) => return Ok(ProjectSyncOutcome::Denied),
        fetched => fetched?,
    };
    match fetched {
        PageFetch::Stored { changed: true, .. } => Ok(ProjectSyncOutcome::Changed),
        PageFetch::Stored { changed: false, .. } | PageFetch::Revalidated(_) => Ok(ProjectSyncOutcome::Unchanged),
        PageFetch::Missing => Ok(ProjectSyncOutcome::Missing),
        PageFetch::Failed {
            status: Some(status), ..
        } => Err(ProjectSyncError::Status(status)),
        PageFetch::Failed {
            error, status: None, ..
        } => Err(error.into()),
    }
}

#[cfg(test)]
#[path = "../../tests/unit/cache/project_sync/tests.rs"]
mod tests;
