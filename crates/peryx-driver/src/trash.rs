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
mod tests {
    use super::{DEFAULT_LIMIT, TrashItem, TrashQuery, TrashQueryError, TrashRef, paginate};
    use peryx_core::{Ecosystem, TRASH_GRACE_SECS, TrashRecord, TrashState};

    fn record(repository: &str, name: &str, deleted_at_unix: i64, retained: bool) -> TrashRecord {
        TrashRecord {
            ecosystem: Ecosystem::new("example"),
            repository: repository.to_owned(),
            name: name.to_owned(),
            reference: Some(format!("{name}.bin")),
            digest: Some("sha256:abc".to_owned()),
            reason: None,
            actor: Some("alice".to_owned()),
            deleted_at_unix,
            retained,
        }
    }

    fn query(limit: usize) -> TrashQuery {
        TrashQuery {
            limit,
            ..TrashQuery::default()
        }
    }

    #[test]
    fn test_default_query_carries_the_default_page_size() {
        assert_eq!(TrashQuery::default().limit, DEFAULT_LIMIT);
    }

    #[test]
    fn test_validate_rejects_bad_limit_cursor_and_repository() {
        assert_eq!(query(0).validate(), Err(TrashQueryError::InvalidLimit));
        assert_eq!(query(101).validate(), Err(TrashQueryError::InvalidLimit));
        assert_eq!(
            TrashQuery {
                cursor: Some(String::new()),
                ..query(25)
            }
            .validate(),
            Err(TrashQueryError::InvalidCursor)
        );
        assert_eq!(
            TrashQuery {
                cursor: Some("x".repeat(1_025)),
                ..query(25)
            }
            .validate(),
            Err(TrashQueryError::InvalidCursor)
        );
        assert_eq!(
            TrashQuery {
                repository: Some("r".repeat(513)),
                ..query(25)
            }
            .validate(),
            Err(TrashQueryError::RepositoryTooLong)
        );
        assert_eq!(query(25).validate(), Ok(()));
    }

    #[test]
    fn test_error_messages_are_actionable() {
        assert_eq!(
            TrashQueryError::InvalidLimit.to_string(),
            "limit must be between 1 and 100"
        );
        assert_eq!(TrashQueryError::InvalidCursor.to_string(), "invalid trash cursor");
        assert_eq!(
            TrashQueryError::RepositoryTooLong.to_string(),
            "repository filter exceeds 512 bytes"
        );
        assert_eq!(TrashQueryError::Store("boom".to_owned()).to_string(), "boom");
    }

    #[test]
    fn test_paginate_orders_newest_first_and_derives_state() {
        let records = vec![
            record("hosted", "old", 1_000, true),
            record("hosted", "new", 2_000, true),
        ];

        let page = paginate(records, &query(25), 2_000);

        assert_eq!(page.next_cursor, None);
        let names: Vec<&str> = page.items.iter().map(|item| item.record.name.as_str()).collect();
        assert_eq!(names, vec!["new", "old"], "newest deletion leads");
        assert!(page.items[0].restorable);
        assert_eq!(page.items[0].state, TrashState::Restorable);
        assert_eq!(page.items[0].deadline_unix, 2_000 + TRASH_GRACE_SECS);
    }

    #[test]
    fn test_paginate_cursor_resumes_after_the_last_row_and_stays_stable() {
        let records = vec![
            record("hosted", "a", 3_000, true),
            record("hosted", "b", 2_000, true),
            record("hosted", "c", 1_000, true),
        ];

        let first = paginate(records.clone(), &query(2), 3_000);
        assert_eq!(
            first
                .items
                .iter()
                .map(|item| item.record.name.clone())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        let cursor = first.next_cursor.expect("a third row remains");

        // A newer artifact entering trash does not shift the boundary the cursor names.
        let mut grown = records;
        grown.push(record("hosted", "z", 4_000, true));
        let second = paginate(
            grown,
            &TrashQuery {
                cursor: Some(cursor),
                ..query(2)
            },
            4_000,
        );

        assert_eq!(
            second
                .items
                .iter()
                .map(|item| item.record.name.clone())
                .collect::<Vec<_>>(),
            vec!["c"],
            "the resumed page holds despite the insertion"
        );
        assert_eq!(second.next_cursor, None);
    }

    #[test]
    fn test_paginate_filters_by_repository_ecosystem_state_and_deadline() {
        let records = vec![
            record("hosted", "keep", 1_000, true),
            record("other", "wrong-repo", 1_000, true),
            TrashRecord {
                ecosystem: Ecosystem::new("other"),
                ..record("hosted", "wrong-eco", 1_000, true)
            },
            record("hosted", "expired", 1_000, false),
        ];

        let restorable = paginate(
            records.clone(),
            &TrashQuery {
                repository: Some("hosted".to_owned()),
                ecosystem: Some(Ecosystem::new("example")),
                state: Some(TrashState::Restorable),
                ..query(25)
            },
            1_000,
        );
        assert_eq!(
            restorable
                .items
                .iter()
                .map(|item| item.record.name.clone())
                .collect::<Vec<_>>(),
            vec!["keep"]
        );

        let expiring = paginate(
            records,
            &TrashQuery {
                deadline_before_unix: Some(1_000 + TRASH_GRACE_SECS),
                ..query(25)
            },
            1_000,
        );
        assert_eq!(expiring.items.len(), 4, "every record's deadline is at the cutoff");
    }

    #[test]
    fn test_trash_ref_matches_only_the_named_record() {
        let record = record("hosted", "flask", 1_000, true);
        let reference = TrashRef {
            ecosystem: Ecosystem::new("example"),
            repository: "hosted".to_owned(),
            name: "flask".to_owned(),
            reference: Some("flask.bin".to_owned()),
            digest: Some("sha256:abc".to_owned()),
        };
        assert!(reference.matches(&record));
        assert!(
            !TrashRef {
                name: "other".to_owned(),
                ..reference
            }
            .matches(&record)
        );
    }

    #[test]
    fn test_trash_item_new_derives_expired_for_unretained_content() {
        let item = TrashItem::new(record("hosted", "flask", 1_000, false), 1_000);
        assert!(!item.restorable);
        assert_eq!(item.state, TrashState::Expired);
    }
}
