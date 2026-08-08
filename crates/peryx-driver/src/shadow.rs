//! The neutral shadowed-candidate query.
//!
//! A virtual repository resolves each project by letting one member win each filename and shadowing
//! the rest. This asks the driver that owns the repository's ecosystem to replay that resolution over
//! stored records (via
//! [`shadowed_candidates`](crate::serving::EcosystemDriver::shadowed_candidates)), then orders the
//! candidates on a stable identity cursor and cuts the requested page. No ecosystem format is named
//! here.

use peryx_core::ShadowCandidate;

use crate::state::AppState;

const MAX_LIMIT: usize = 100;
const DEFAULT_LIMIT: usize = 25;
const MAX_PROJECT_BYTES: usize = 512;
const MAX_CURSOR_BYTES: usize = 1024;

/// What an operator asked to explain: a virtual repository, a project, a page size, and an exclusive
/// cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowQuery {
    pub repository: String,
    pub project: String,
    pub cursor: Option<String>,
    pub limit: usize,
}

impl ShadowQuery {
    /// A query for one repository and project with default paging.
    #[must_use]
    pub const fn new(repository: String, project: String) -> Self {
        Self {
            repository,
            project,
            cursor: None,
            limit: DEFAULT_LIMIT,
        }
    }

    /// Validate pagination and the bounded project filter without reading storage.
    ///
    /// # Errors
    /// Returns the first invalid limit, cursor, or oversized project.
    pub fn validate(&self) -> Result<(), ShadowQueryError> {
        if !(1..=MAX_LIMIT).contains(&self.limit) {
            return Err(ShadowQueryError::InvalidLimit);
        }
        if self
            .cursor
            .as_ref()
            .is_some_and(|cursor| cursor.is_empty() || cursor.len() > MAX_CURSOR_BYTES)
        {
            return Err(ShadowQueryError::InvalidCursor);
        }
        if self.project.len() > MAX_PROJECT_BYTES {
            return Err(ShadowQueryError::ProjectTooLong);
        }
        Ok(())
    }
}

/// Why a shadowed-candidate query could not run: a bad page request, or a member the scan could not
/// read.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ShadowQueryError {
    #[error("limit must be between 1 and {MAX_LIMIT}")]
    InvalidLimit,
    #[error("invalid shadow cursor")]
    InvalidCursor,
    #[error("project filter exceeds {MAX_PROJECT_BYTES} bytes")]
    ProjectTooLong,
    #[error("{0}")]
    Store(String),
}

/// A page of resolution candidates, ordered by filename then selection, with the cursor that resumes
/// after the last row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowPage {
    pub candidates: Vec<ShadowCandidate>,
    pub next_cursor: Option<String>,
}

impl AppState {
    /// Replay one virtual repository's resolution of a project and page its shadowed candidates.
    ///
    /// A repository the caller already authorized that resolves nothing - a non-virtual index, an
    /// unknown project, or an ecosystem without a driver - yields an empty page rather than an error.
    ///
    /// # Errors
    /// Returns a validation error for a bad limit, cursor, or project, or a store error when the
    /// driver's resolution scan fails.
    pub fn query_shadowed(&self, query: &ShadowQuery) -> Result<ShadowPage, ShadowQueryError> {
        query.validate()?;
        let candidates = if let Some(position) = self.indexes.iter().position(|index| index.name == query.repository)
            && let Some(driver) = self
                .driver_for(self.index_at(position).ecosystem)
                .and_then(|driver| driver.capabilities().shadow)
        {
            driver
                .shadowed_candidates(self.serving.as_ref(), position, &query.project)
                .map_err(ShadowQueryError::Store)?
        } else {
            Vec::new()
        };
        Ok(paginate(candidates, query))
    }
}

/// Order the candidates on their stable identity cursor and cut the requested page. Splitting this
/// from the driver read keeps the pagination invariant unit-testable.
fn paginate(mut candidates: Vec<ShadowCandidate>, query: &ShadowQuery) -> ShadowPage {
    candidates.sort_by_key(ShadowCandidate::cursor);
    let start = query.cursor.as_deref().map_or(0, |cursor| {
        candidates.partition_point(|candidate| candidate.cursor().as_str() <= cursor)
    });
    let page = &candidates[start..];
    let next_cursor = page.get(query.limit).map(|_| page[query.limit - 1].cursor());
    let candidates = page.iter().take(query.limit).cloned().collect();
    ShadowPage {
        candidates,
        next_cursor,
    }
}

#[cfg(test)]
#[path = "../tests/unit/shadow/tests.rs"]
mod tests;
