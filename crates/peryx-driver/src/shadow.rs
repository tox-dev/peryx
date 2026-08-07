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
mod tests {
    use peryx_core::{ShadowCandidate, ShadowReason, ShadowSource};

    use super::{DEFAULT_LIMIT, ShadowQuery, ShadowQueryError, paginate};

    fn candidate(filename: &str, member: &str, selected: bool) -> ShadowCandidate {
        ShadowCandidate {
            repository: "root/alpha".to_owned(),
            project: "flask".to_owned(),
            member: member.to_owned(),
            source: if selected {
                ShadowSource::Hosted
            } else {
                ShadowSource::Cached
            },
            filename: filename.to_owned(),
            digest: Some("sha256:abc".to_owned()),
            selected,
            reason: (!selected).then_some(ShadowReason::Precedence),
        }
    }

    fn query(limit: usize) -> ShadowQuery {
        ShadowQuery {
            limit,
            ..ShadowQuery::new("root/alpha".to_owned(), "flask".to_owned())
        }
    }

    #[test]
    fn test_new_query_carries_the_default_page_size() {
        assert_eq!(ShadowQuery::new(String::new(), String::new()).limit, DEFAULT_LIMIT);
    }

    #[test]
    fn test_validate_rejects_bad_limit_cursor_and_project() {
        assert_eq!(query(0).validate(), Err(ShadowQueryError::InvalidLimit));
        assert_eq!(query(101).validate(), Err(ShadowQueryError::InvalidLimit));
        assert_eq!(
            ShadowQuery {
                cursor: Some(String::new()),
                ..query(25)
            }
            .validate(),
            Err(ShadowQueryError::InvalidCursor)
        );
        assert_eq!(
            ShadowQuery {
                cursor: Some("x".repeat(1_025)),
                ..query(25)
            }
            .validate(),
            Err(ShadowQueryError::InvalidCursor)
        );
        assert_eq!(
            ShadowQuery {
                project: "p".repeat(513),
                ..query(25)
            }
            .validate(),
            Err(ShadowQueryError::ProjectTooLong)
        );
        assert_eq!(query(25).validate(), Ok(()));
    }

    #[test]
    fn test_error_messages_are_actionable() {
        assert_eq!(
            ShadowQueryError::InvalidLimit.to_string(),
            "limit must be between 1 and 100"
        );
        assert_eq!(ShadowQueryError::InvalidCursor.to_string(), "invalid shadow cursor");
        assert_eq!(
            ShadowQueryError::ProjectTooLong.to_string(),
            "project filter exceeds 512 bytes"
        );
        assert_eq!(ShadowQueryError::Store("boom".to_owned()).to_string(), "boom");
    }

    #[test]
    fn test_paginate_orders_by_filename_then_selection() {
        let candidates = vec![
            candidate("flask-1.0.bin", "alpha", false),
            candidate("flask-1.0.bin", "hosted", true),
            candidate("flask-2.0.bin", "hosted", true),
        ];

        let page = paginate(candidates, &query(25));

        assert_eq!(page.next_cursor, None);
        let rows: Vec<(&str, &str)> = page
            .candidates
            .iter()
            .map(|candidate| (candidate.filename.as_str(), candidate.member.as_str()))
            .collect();
        assert_eq!(
            rows,
            vec![
                ("flask-1.0.bin", "hosted"),
                ("flask-1.0.bin", "alpha"),
                ("flask-2.0.bin", "hosted")
            ],
            "the selected candidate leads its filename group"
        );
    }

    #[test]
    fn test_paginate_cursor_resumes_after_the_last_row_and_stays_stable() {
        let candidates = vec![
            candidate("a.bin", "hosted", true),
            candidate("b.bin", "hosted", true),
            candidate("c.bin", "hosted", true),
        ];

        let first = paginate(candidates.clone(), &query(2));
        assert_eq!(
            first
                .candidates
                .iter()
                .map(|candidate| candidate.filename.clone())
                .collect::<Vec<_>>(),
            vec!["a.bin", "b.bin"]
        );
        let cursor = first.next_cursor.expect("a third row remains");

        let second = paginate(
            candidates,
            &ShadowQuery {
                cursor: Some(cursor),
                ..query(2)
            },
        );
        assert_eq!(
            second
                .candidates
                .iter()
                .map(|candidate| candidate.filename.clone())
                .collect::<Vec<_>>(),
            vec!["c.bin"],
            "the resumed page holds without skipping or duplicating a candidate"
        );
        assert_eq!(second.next_cursor, None);
    }
}
