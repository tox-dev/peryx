//! The neutral trash-inspection query.
//!
//! Each ecosystem driver reads its own soft-delete keyspace through
//! [`trash_records`](crate::serving::EcosystemDriver::trash_records); this merges those neutral
//! records, applies the operator's filters, derives restorable state against one clock read, and
//! pages the result on a stable identity cursor. No ecosystem format is named here.

use std::collections::HashMap;

use peryx_core::{Ecosystem, TrashRecord, TrashState};

use crate::state::AppState;

const MAX_LIMIT: usize = 100;
const DEFAULT_LIMIT: usize = 25;
const MAX_REPOSITORY_BYTES: usize = 512;
const MAX_CURSOR_BYTES: usize = 1024;

/// What an operator asked to see: optional filters, a page size, and an exclusive cursor.
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

/// Why a trash query could not run: a bad page request, or a store the scan could not read.
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
            .is_none_or(|repository| record.repository == repository)
            && self.ecosystem.is_none_or(|ecosystem| record.ecosystem == ecosystem)
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
    pub repository: String,
    pub name: String,
    pub reference: Option<String>,
    pub digest: Option<String>,
}

impl TrashRef {
    fn matches(&self, record: &TrashRecord) -> bool {
        record.ecosystem == self.ecosystem
            && record.repository == self.repository
            && record.name == self.name
            && record.reference == self.reference
            && record.digest == self.digest
    }
}

impl AppState {
    /// Merge, filter, and page every ecosystem's trash records for one operator query.
    ///
    /// # Errors
    /// Returns a validation error for a bad limit, cursor, or filter, or a store error when an
    /// ecosystem's trash scan fails.
    pub fn query_trash(&self, query: &TrashQuery) -> Result<TrashPage, TrashQueryError> {
        query.validate()?;
        let now_unix = (self.clock)();
        let records = self.collect_trash(query.repository.as_deref(), query.ecosystem)?;
        Ok(paginate(records, query, now_unix))
    }

    /// The one trashed record matching `reference`, with its derived state, or `None`.
    ///
    /// # Errors
    /// Returns a store error when the ecosystem's trash scan fails.
    pub fn inspect_trash(&self, reference: &TrashRef) -> Result<Option<TrashItem>, TrashQueryError> {
        let now_unix = (self.clock)();
        Ok(self
            .collect_trash(Some(&reference.repository), Some(reference.ecosystem))?
            .into_iter()
            .find(|record| reference.matches(record))
            .map(|record| TrashItem::new(record, now_unix)))
    }

    /// Read trash records from each driver whose ecosystem and index the filters admit, so a scoped
    /// query never asks a driver about indexes it excludes.
    fn collect_trash(
        &self,
        repository: Option<&str>,
        ecosystem: Option<Ecosystem>,
    ) -> Result<Vec<TrashRecord>, TrashQueryError> {
        let mut by_ecosystem: HashMap<Ecosystem, Vec<String>> = HashMap::new();
        for index in &self.indexes {
            if ecosystem.is_some_and(|wanted| index.ecosystem != wanted)
                || repository.is_some_and(|wanted| index.name != wanted)
            {
                continue;
            }
            by_ecosystem
                .entry(index.ecosystem)
                .or_default()
                .push(index.name.clone());
        }
        let mut records = Vec::new();
        for (ecosystem, names) in by_ecosystem {
            if let Some(driver) = self
                .driver_for(ecosystem)
                .and_then(|driver| driver.capabilities().trash)
            {
                records.extend(
                    driver
                        .trash_records(&self.meta, &names)
                        .map_err(TrashQueryError::Store)?,
                );
            }
        }
        Ok(records)
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
