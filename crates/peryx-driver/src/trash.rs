//! Neutral soft-delete queries over owner records.

use std::collections::HashMap;

use peryx_core::{ArtifactKey, Ecosystem, RepositoryKey, ResourceKey, TrashRecord, TrashState};

use crate::driver_set::DriverSet;
use crate::state::{AppState, ServingState};

const MAX_LIMIT: usize = 100;
const DEFAULT_LIMIT: usize = 25;
const MAX_REPOSITORY_BYTES: usize = 512;
const MAX_CURSOR_BYTES: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashQuery {
    pub repository: Option<String>,
    pub ecosystem: Option<Ecosystem>,
    pub state: Option<TrashState>,
    /// Keep only records whose recovery deadline is at or before this Unix time, to surface expiring
    /// entries first.
    pub deadline_before_unix: Option<i64>,
    pub cursor: Option<String>,
    pub limit: usize,
}

impl Default for TrashQuery {
    fn default() -> Self {
        Self {
            repository: None,
            ecosystem: None,
            state: None,
            deadline_before_unix: None,
            cursor: None,
            limit: DEFAULT_LIMIT,
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TrashQueryError {
    #[error("limit must be between 1 and {MAX_LIMIT}")]
    InvalidLimit,
    #[error("invalid trash cursor")]
    InvalidCursor,
    #[error("repository filter exceeds {MAX_REPOSITORY_BYTES} bytes")]
    RepositoryTooLong,
    #[error("{0}")]
    Store(String),
}

impl TrashQuery {
    /// Validate pagination and the bounded repository filter without reading storage.
    ///
    /// # Errors
    /// Returns the first invalid limit, cursor, or oversized repository filter.
    pub fn validate(&self) -> Result<(), TrashQueryError> {
        if !(1..=MAX_LIMIT).contains(&self.limit) {
            return Err(TrashQueryError::InvalidLimit);
        }
        if self
            .cursor
            .as_ref()
            .is_some_and(|cursor| cursor.is_empty() || cursor.len() > MAX_CURSOR_BYTES)
        {
            return Err(TrashQueryError::InvalidCursor);
        }
        if self
            .repository
            .as_ref()
            .is_some_and(|repository| repository.len() > MAX_REPOSITORY_BYTES)
        {
            return Err(TrashQueryError::RepositoryTooLong);
        }
        Ok(())
    }

    fn matches(&self, record: &TrashRecord, now_unix: i64) -> bool {
        self.repository
            .as_deref()
            .is_none_or(|repository| record.repository.as_str() == repository)
            && self
                .ecosystem
                .as_ref()
                .is_none_or(|ecosystem| &record.ecosystem == ecosystem)
            && self.state.is_none_or(|state| record.state(now_unix) == state)
            && self
                .deadline_before_unix
                .is_none_or(|deadline| record.deadline_unix() <= deadline)
    }
}

/// One trashed artifact plus its state derived at query time, so the API and the UI show identical
/// restorability without recomputing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashItem {
    pub record: TrashRecord,
    pub state: TrashState,
    pub deadline_unix: i64,
    pub restorable: bool,
}

impl TrashItem {
    const fn new(record: TrashRecord, now_unix: i64) -> Self {
        Self {
            state: record.state(now_unix),
            deadline_unix: record.deadline_unix(),
            restorable: record.restorable(now_unix),
            record,
        }
    }
}

/// A page of trashed records, newest first, with the cursor that resumes after the last row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashPage {
    pub items: Vec<TrashItem>,
    pub next_cursor: Option<String>,
}

/// Identifies one trashed record for the inspect route, by the same fields its cursor is built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashRef {
    pub ecosystem: Ecosystem,
    pub repository: RepositoryKey,
    pub resource: ResourceKey,
    pub artifact: Option<ArtifactKey>,
    pub digest: Option<String>,
}

impl TrashRef {
    fn matches(&self, record: &TrashRecord) -> bool {
        record.ecosystem == self.ecosystem
            && record.repository == self.repository
            && record.resource == self.resource
            && record.artifact == self.artifact
            && record.digest == self.digest
    }
}

pub struct TrashServices {
    serving: std::sync::Arc<ServingState>,
    drivers: DriverSet,
}

impl TrashServices {
    #[must_use]
    pub fn for_state(state: &AppState) -> Self {
        Self {
            serving: std::sync::Arc::clone(&state.serving),
            drivers: state.driver_set().clone(),
        }
    }

    /// # Errors
    /// Returns a validation error for a bad limit, cursor, or filter, or a store error when an
    /// ecosystem's trash scan fails.
    pub fn query(&self, query: &TrashQuery) -> Result<TrashPage, TrashQueryError> {
        query.validate()?;
        let now_unix = (self.serving.clock)();
        let records = self.collect_trash(query.repository.as_deref(), query.ecosystem.as_ref())?;
        Ok(paginate(records, query, now_unix))
    }

    /// # Errors
    /// Returns a store error when the ecosystem's trash scan fails.
    pub fn inspect(&self, reference: &TrashRef) -> Result<Option<TrashItem>, TrashQueryError> {
        let now_unix = (self.serving.clock)();
        Ok(self
            .collect_trash(Some(reference.repository.as_str()), Some(&reference.ecosystem))?
            .into_iter()
            .find(|record| reference.matches(record))
            .map(|record| TrashItem::new(record, now_unix)))
    }

    /// Read trash records from each driver whose ecosystem and index the filters admit, so a scoped
    /// query never asks a driver about indexes it excludes.
    fn collect_trash(
        &self,
        repository: Option<&str>,
        ecosystem: Option<&Ecosystem>,
    ) -> Result<Vec<TrashRecord>, TrashQueryError> {
        let mut by_ecosystem: HashMap<Ecosystem, Vec<String>> = HashMap::new();
        for index in &self.serving.indexes {
            if ecosystem.is_some_and(|wanted| &index.ecosystem != wanted)
                || repository.is_some_and(|wanted| index.name != wanted)
            {
                continue;
            }
            by_ecosystem
                .entry(index.ecosystem.clone())
                .or_default()
                .push(index.name.clone());
        }
        let mut records = Vec::new();
        for (ecosystem, names) in by_ecosystem {
            if let Some(driver) = self.drivers.get_trash(&ecosystem) {
                records.extend(
                    driver
                        .trash_records(&self.serving.meta, &names)
                        .map_err(TrashQueryError::Store)?,
                );
            }
        }
        Ok(records)
    }
}

impl crate::http_services::TrashService for TrashServices {
    fn query(&self, query: &TrashQuery) -> Result<TrashPage, TrashQueryError> {
        self.query(query)
    }

    fn inspect(&self, reference: &TrashRef) -> Result<Option<TrashItem>, TrashQueryError> {
        self.inspect(reference)
    }
}

/// Filter to the matching records, order them newest first on a stable identity cursor, and cut the
/// requested page. Splitting this from the store read keeps the pagination invariant unit-testable.
fn paginate(mut records: Vec<TrashRecord>, query: &TrashQuery, now_unix: i64) -> TrashPage {
    records.retain(|record| query.matches(record, now_unix));
    records.sort_by_key(TrashRecord::cursor);
    let start = query.cursor.as_deref().map_or(0, |cursor| {
        records.partition_point(|record| record.cursor().as_str() <= cursor)
    });
    let page = &records[start..];
    let next_cursor = page.get(query.limit).map(|_| page[query.limit - 1].cursor());
    let items = page
        .iter()
        .take(query.limit)
        .map(|record| TrashItem::new(record.clone(), now_unix))
        .collect();
    TrashPage { items, next_cursor }
}

#[cfg(test)]
#[path = "../tests/unit/trash/tests.rs"]
mod tests;
