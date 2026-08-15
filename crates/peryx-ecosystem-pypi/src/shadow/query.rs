use peryx_driver::state::AppState;

use super::ShadowCandidate;

const MAX_LIMIT: usize = 100;
const MAX_PROJECT_BYTES: usize = 512;
const MAX_CURSOR_BYTES: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowQuery {
    pub repository: String,
    pub project: String,
    pub cursor: Option<String>,
    pub limit: usize,
}

impl ShadowQuery {
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

/// # Errors
/// Returns invalid input or a stored-page read failure.
pub fn query_shadowed(state: &AppState, query: &ShadowQuery) -> Result<ShadowPage, ShadowQueryError> {
    query.validate()?;
    let candidates = if let Some(position) = state
        .serving
        .indexes
        .iter()
        .position(|index| index.name == query.repository && index.ecosystem == crate::ECOSYSTEM)
    {
        crate::cache::shadowed_candidates(state.serving.as_ref(), position, &query.project)
            .map_err(ShadowQueryError::Store)?
    } else {
        Vec::new()
    };
    Ok(paginate(candidates, query))
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
