//! [`RepositoryId`] values survive display-name changes, while unique routes identify repositories on
//! the wire. The store keeps definitions as opaque JSON; ecosystem crates own their shape and
//! validation. Each mutation uses one transaction and advances the version used for precondition checks.

use std::collections::BTreeSet;
use std::fmt;
use std::ops::Bound::{Excluded, Unbounded};

use peryx_identity::UserId;
use redb::ReadableTable as _;
use serde::{Deserialize, Serialize};

use super::{MetaError, MetaStore, REPOSITORY, REPOSITORY_ROUTE};

const MAX_QUERY_LIMIT: usize = 100;
const MAX_ROUTE_BYTES: usize = 512;
const MAX_DISPLAY_NAME_BYTES: usize = 256;
const MAX_ECOSYSTEM_BYTES: usize = 64;

/// Stable across display-name and definition changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RepositoryId(String);

impl RepositoryId {
    #[must_use]
    pub fn random() -> Self {
        Self(format!("repo_{}", uuid::Uuid::new_v4().simple()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RepositoryId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Disabling keeps the record and can be reversed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryState {
    Enabled,
    Disabled,
}

/// `route`, `ecosystem`, and `id` are immutable. The store does not interpret `definition`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryRecord {
    pub id: RepositoryId,
    pub route: String,
    pub display_name: String,
    pub ecosystem: String,
    pub definition: serde_json::Value,
    pub state: RepositoryState,
    pub version: u64,
    pub created_by: UserId,
    pub created_at_unix: i64,
    pub updated_by: UserId,
    pub updated_at_unix: i64,
}

/// The store assigns the ID, version, state, and timestamps.
#[derive(Debug)]
pub struct NewRepository {
    pub route: String,
    pub display_name: String,
    pub ecosystem: String,
    pub definition: serde_json::Value,
    pub created_by: UserId,
}

/// `route` and `ecosystem` are immutable.
#[derive(Debug)]
pub struct RepositoryUpdate {
    pub display_name: String,
    pub definition: serde_json::Value,
}

/// Field validation runs before opening a transaction.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RepositoryFieldError {
    #[error("repository route must not be empty")]
    EmptyRoute,
    #[error("repository route exceeds {MAX_ROUTE_BYTES} bytes")]
    RouteTooLong,
    #[error("repository display name must not be empty")]
    EmptyDisplayName,
    #[error("repository display name exceeds {MAX_DISPLAY_NAME_BYTES} bytes")]
    DisplayNameTooLong,
    #[error("repository ecosystem must not be empty")]
    EmptyEcosystem,
    #[error("repository ecosystem exceeds {MAX_ECOSYSTEM_BYTES} bytes")]
    EcosystemTooLong,
}

#[derive(Debug, thiserror::Error)]
pub enum CreateRepositoryError {
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error(transparent)]
    Field(#[from] RepositoryFieldError),
    #[error("route {route} is already taken by another repository")]
    DuplicateRoute { route: String },
}

/// `VersionConflict` reports the stored version for refetch and retry.
#[derive(Debug, thiserror::Error)]
pub enum UpdateRepositoryError {
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error(transparent)]
    Field(#[from] RepositoryFieldError),
    #[error("no repository holds this identifier")]
    NotFound,
    #[error("repository is at version {current}, not the expected version")]
    VersionConflict { current: u64 },
}

#[derive(Debug, thiserror::Error)]
pub enum RepositoryStateError {
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error("no repository holds this identifier")]
    NotFound,
    #[error("repository is at version {current}, not the expected version")]
    VersionConflict { current: u64 },
}

/// A bounded query in identifier order with an exclusive cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryQuery {
    pub state: Option<RepositoryState>,
    pub cursor: Option<RepositoryId>,
    pub limit: usize,
}

impl Default for RepositoryQuery {
    fn default() -> Self {
        Self {
            state: None,
            cursor: None,
            limit: 25,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepositoryPage {
    pub repositories: Vec<RepositoryRecord>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum RepositoryQueryError {
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error("limit must be between 1 and {MAX_QUERY_LIMIT}")]
    InvalidLimit,
}

/// Reconciliation matches by route and preserves an existing record's ID and version lineage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredRepository {
    pub route: String,
    pub display_name: String,
    pub ecosystem: String,
    pub definition: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileAction {
    Created,
    Updated,
    Unchanged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconciledRepository {
    pub record: RepositoryRecord,
    pub action: ReconcileAction,
}

#[derive(Debug, thiserror::Error)]
pub enum ReconcileRepositoryError {
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error(transparent)]
    Field(#[from] RepositoryFieldError),
    #[error("route {route} appears more than once in the desired set")]
    DuplicateRoute { route: String },
    #[error("route {route} is registered to ecosystem {found}, not {desired}")]
    EcosystemChanged {
        route: String,
        found: String,
        desired: String,
    },
}

impl MetaStore {
    /// Creates version 1 only when the route is unclaimed.
    ///
    /// # Errors
    /// Returns [`RepositoryFieldError`] for an empty or oversized field,
    /// [`CreateRepositoryError::DuplicateRoute`] for a claimed route, or a store error when encoding or
    /// committing fails.
    pub fn create_repository(&self, new: NewRepository, now: i64) -> Result<RepositoryRecord, CreateRepositoryError> {
        validate_route(&new.route)?;
        validate_display_name(&new.display_name)?;
        validate_ecosystem(&new.ecosystem)?;
        let txn = self.db.begin_write().map_err(MetaError::from)?;
        if txn
            .open_table(REPOSITORY_ROUTE)
            .map_err(MetaError::from)?
            .get(new.route.as_str())
            .map_err(MetaError::from)?
            .is_some()
        {
            return Err(CreateRepositoryError::DuplicateRoute { route: new.route });
        }
        let record = RepositoryRecord {
            id: RepositoryId::random(),
            route: new.route,
            display_name: new.display_name,
            ecosystem: new.ecosystem,
            definition: new.definition,
            state: RepositoryState::Enabled,
            version: 1,
            created_by: new.created_by.clone(),
            created_at_unix: now,
            updated_by: new.created_by,
            updated_at_unix: now,
        };
        let encoded = serde_json::to_vec(&record).map_err(MetaError::from)?;
        txn.open_table(REPOSITORY)
            .map_err(MetaError::from)?
            .insert(record.id.as_str(), encoded.as_slice())
            .map_err(MetaError::from)?;
        txn.open_table(REPOSITORY_ROUTE)
            .map_err(MetaError::from)?
            .insert(record.route.as_str(), record.id.as_str())
            .map_err(MetaError::from)?;
        txn.commit().map_err(MetaError::from)?;
        Ok(record)
    }

    /// # Errors
    /// Returns a store error when reading or decoding fails.
    pub fn repository(&self, id: &RepositoryId) -> Result<Option<RepositoryRecord>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(REPOSITORY)?;
        Ok(table
            .get(id.as_str())?
            .map(|value| serde_json::from_slice(value.value()))
            .transpose()?)
    }

    /// # Errors
    /// Returns a store error when reading or decoding fails.
    pub fn repository_by_route(&self, route: &str) -> Result<Option<RepositoryRecord>, MetaError> {
        let txn = self.db.begin_read()?;
        let id = txn
            .open_table(REPOSITORY_ROUTE)?
            .get(route)?
            .map(|value| value.value().to_owned());
        let Some(id) = id else {
            return Ok(None);
        };
        Ok(txn
            .open_table(REPOSITORY)?
            .get(id.as_str())?
            .map(|value| serde_json::from_slice(value.value()))
            .transpose()?)
    }

    /// Requires `expected_version` to match and preserves the ID, route, and ecosystem.
    ///
    /// # Errors
    /// Returns [`RepositoryFieldError`] for an invalid display name, [`UpdateRepositoryError::NotFound`]
    /// for an unknown ID, [`UpdateRepositoryError::VersionConflict`] for a stale precondition, or a
    /// store error when reading, encoding, or committing fails.
    pub fn update_repository(
        &self,
        id: &RepositoryId,
        expected_version: u64,
        update: RepositoryUpdate,
        actor: &UserId,
        now: i64,
    ) -> Result<RepositoryRecord, UpdateRepositoryError> {
        validate_display_name(&update.display_name)?;
        let txn = self.db.begin_write().map_err(MetaError::from)?;
        let mut record = load_for_update(&txn, id)?.ok_or(UpdateRepositoryError::NotFound)?;
        if record.version != expected_version {
            return Err(UpdateRepositoryError::VersionConflict {
                current: record.version,
            });
        }
        record.display_name = update.display_name;
        record.definition = update.definition;
        record.version += 1;
        record.updated_by = actor.clone();
        record.updated_at_unix = now;
        write_record(&txn, &record)?;
        txn.commit().map_err(MetaError::from)?;
        Ok(record)
    }

    /// Requires `expected_version` to match. Repeating the current state returns the record unchanged;
    /// a transition advances the version.
    ///
    /// # Errors
    /// Returns [`RepositoryStateError::NotFound`] for an unknown ID,
    /// [`RepositoryStateError::VersionConflict`] for a stale precondition, or a store error when
    /// reading, encoding, or committing fails.
    pub fn set_repository_enabled(
        &self,
        id: &RepositoryId,
        expected_version: u64,
        enabled: bool,
        actor: &UserId,
        now: i64,
    ) -> Result<RepositoryRecord, RepositoryStateError> {
        let txn = self.db.begin_write().map_err(MetaError::from)?;
        let mut record = load_for_update(&txn, id)?.ok_or(RepositoryStateError::NotFound)?;
        if record.version != expected_version {
            return Err(RepositoryStateError::VersionConflict {
                current: record.version,
            });
        }
        let desired = if enabled {
            RepositoryState::Enabled
        } else {
            RepositoryState::Disabled
        };
        if record.state == desired {
            return Ok(record);
        }
        record.state = desired;
        record.version += 1;
        record.updated_by = actor.clone();
        record.updated_at_unix = now;
        write_record(&txn, &record)?;
        txn.commit().map_err(MetaError::from)?;
        Ok(record)
    }

    /// Lists repositories in ID order after an exclusive cursor, with an optional state filter.
    ///
    /// # Errors
    /// Returns [`RepositoryQueryError::InvalidLimit`] for a limit outside 1 through 100, or a store
    /// error when reading or decoding fails.
    pub fn list_repositories(&self, query: &RepositoryQuery) -> Result<RepositoryPage, RepositoryQueryError> {
        if !(1..=MAX_QUERY_LIMIT).contains(&query.limit) {
            return Err(RepositoryQueryError::InvalidLimit);
        }
        let txn = self.db.begin_read().map_err(MetaError::from)?;
        let table = txn.open_table(REPOSITORY).map_err(MetaError::from)?;
        let cursor = query.cursor.as_ref().map(|cursor| cursor.as_str().to_owned());
        let entries = cursor
            .as_ref()
            .map_or_else(
                || table.iter(),
                |cursor| table.range::<&str>((Excluded(cursor.as_str()), Unbounded)),
            )
            .map_err(MetaError::from)?;
        let mut records = Vec::with_capacity(query.limit + 1);
        for entry in entries {
            let (_key, value) = entry.map_err(MetaError::from)?;
            let record: RepositoryRecord = serde_json::from_slice(value.value()).map_err(MetaError::from)?;
            if query.state.is_some_and(|state| record.state != state) {
                continue;
            }
            records.push(record);
            if records.len() > query.limit {
                break;
            }
        }
        let next_cursor = (records.len() > query.limit).then(|| records[query.limit - 1].id.as_str().to_owned());
        records.truncate(query.limit);
        Ok(RepositoryPage {
            repositories: records,
            next_cursor,
        })
    }

    /// # Errors
    /// Returns a store error when a repository record cannot be read or decoded.
    pub fn repository_ecosystems(&self) -> Result<BTreeSet<String>, MetaError> {
        let txn = self.db.begin_read().map_err(MetaError::from)?;
        let table = txn.open_table(REPOSITORY).map_err(MetaError::from)?;
        table
            .iter()
            .map_err(MetaError::from)?
            .map(|entry| {
                let (_key, value) = entry.map_err(MetaError::from)?;
                serde_json::from_slice::<RepositoryRecord>(value.value())
                    .map(|record| record.ecosystem)
                    .map_err(MetaError::from)
            })
            .collect()
    }

    /// Matches records by route. New routes start at version 1, changed records advance their version,
    /// and unchanged or omitted routes retain their records. Ecosystem changes fail the atomic batch.
    ///
    /// # Errors
    /// Returns [`ReconcileRepositoryError::DuplicateRoute`] for duplicate desired routes,
    /// [`ReconcileRepositoryError::EcosystemChanged`] for an ecosystem mismatch,
    /// [`RepositoryFieldError`] for an invalid field, or a store error when reading, encoding, or
    /// committing fails.
    pub fn reconcile_repositories(
        &self,
        desired: &[DesiredRepository],
        actor: &UserId,
        now: i64,
    ) -> Result<Vec<ReconciledRepository>, ReconcileRepositoryError> {
        let mut seen = BTreeSet::new();
        for repository in desired {
            validate_route(&repository.route)?;
            validate_display_name(&repository.display_name)?;
            validate_ecosystem(&repository.ecosystem)?;
            if !seen.insert(repository.route.as_str()) {
                return Err(ReconcileRepositoryError::DuplicateRoute {
                    route: repository.route.clone(),
                });
            }
        }
        let txn = self.db.begin_write().map_err(MetaError::from)?;
        let mut reconciled = Vec::with_capacity(desired.len());
        for repository in desired {
            let existing = txn
                .open_table(REPOSITORY_ROUTE)
                .map_err(MetaError::from)?
                .get(repository.route.as_str())
                .map_err(MetaError::from)?
                .map(|value| RepositoryId(value.value().to_owned()));
            reconciled.push(match existing {
                Some(id) => reconcile_existing(&txn, &id, repository, actor, now)?,
                None => reconcile_new(&txn, repository, actor, now)?,
            });
        }
        txn.commit().map_err(MetaError::from)?;
        Ok(reconciled)
    }
}

fn reconcile_new(
    txn: &redb::WriteTransaction,
    desired: &DesiredRepository,
    actor: &UserId,
    now: i64,
) -> Result<ReconciledRepository, ReconcileRepositoryError> {
    let record = RepositoryRecord {
        id: RepositoryId::random(),
        route: desired.route.clone(),
        display_name: desired.display_name.clone(),
        ecosystem: desired.ecosystem.clone(),
        definition: desired.definition.clone(),
        state: RepositoryState::Enabled,
        version: 1,
        created_by: actor.clone(),
        created_at_unix: now,
        updated_by: actor.clone(),
        updated_at_unix: now,
    };
    write_record(txn, &record)?;
    txn.open_table(REPOSITORY_ROUTE)
        .map_err(MetaError::from)?
        .insert(record.route.as_str(), record.id.as_str())
        .map_err(MetaError::from)?;
    Ok(ReconciledRepository {
        record,
        action: ReconcileAction::Created,
    })
}

fn reconcile_existing(
    txn: &redb::WriteTransaction,
    id: &RepositoryId,
    desired: &DesiredRepository,
    actor: &UserId,
    now: i64,
) -> Result<ReconciledRepository, ReconcileRepositoryError> {
    let mut record = load_for_update(txn, id)?.expect("route index points to a stored repository record");
    if record.ecosystem != desired.ecosystem {
        return Err(ReconcileRepositoryError::EcosystemChanged {
            route: desired.route.clone(),
            found: record.ecosystem,
            desired: desired.ecosystem.clone(),
        });
    }
    if record.display_name == desired.display_name && record.definition == desired.definition {
        return Ok(ReconciledRepository {
            record,
            action: ReconcileAction::Unchanged,
        });
    }
    record.display_name.clone_from(&desired.display_name);
    record.definition.clone_from(&desired.definition);
    record.version += 1;
    record.updated_by = actor.clone();
    record.updated_at_unix = now;
    write_record(txn, &record)?;
    Ok(ReconciledRepository {
        record,
        action: ReconcileAction::Updated,
    })
}

fn load_for_update(txn: &redb::WriteTransaction, id: &RepositoryId) -> Result<Option<RepositoryRecord>, MetaError> {
    Ok(txn
        .open_table(REPOSITORY)?
        .get(id.as_str())?
        .map(|value| serde_json::from_slice(value.value()))
        .transpose()?)
}

fn write_record(txn: &redb::WriteTransaction, record: &RepositoryRecord) -> Result<(), MetaError> {
    let encoded = serde_json::to_vec(record)?;
    txn.open_table(REPOSITORY)?
        .insert(record.id.as_str(), encoded.as_slice())?;
    Ok(())
}

const fn validate_route(route: &str) -> Result<(), RepositoryFieldError> {
    if route.is_empty() {
        return Err(RepositoryFieldError::EmptyRoute);
    }
    if route.len() > MAX_ROUTE_BYTES {
        return Err(RepositoryFieldError::RouteTooLong);
    }
    Ok(())
}

const fn validate_display_name(display_name: &str) -> Result<(), RepositoryFieldError> {
    if display_name.is_empty() {
        return Err(RepositoryFieldError::EmptyDisplayName);
    }
    if display_name.len() > MAX_DISPLAY_NAME_BYTES {
        return Err(RepositoryFieldError::DisplayNameTooLong);
    }
    Ok(())
}

const fn validate_ecosystem(ecosystem: &str) -> Result<(), RepositoryFieldError> {
    if ecosystem.is_empty() {
        return Err(RepositoryFieldError::EmptyEcosystem);
    }
    if ecosystem.len() > MAX_ECOSYSTEM_BYTES {
        return Err(RepositoryFieldError::EcosystemTooLong);
    }
    Ok(())
}
