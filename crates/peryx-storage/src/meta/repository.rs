//! Persistent repository records: the durable definition a management API creates, inspects, lists,
//! updates, and disables, versioned so an update can carry a precondition.
//!
//! A repository is identified by an opaque [`RepositoryId`] that survives display-name changes, and
//! addressed on the wire by a unique `route`. The `definition` is an ecosystem-agnostic JSON envelope
//! the store never interprets - the format-specific shape and its validation live in the ecosystem
//! crates, so this neutral store grows no per-ecosystem tables. Each mutation bumps a monotonic
//! [`version`](RepositoryRecord::version), the strong validator an update or disable checks its
//! precondition against, and the whole change commits in one redb transaction.

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

/// An opaque repository identifier that stays stable when a repository's display name or definition
/// changes, so a rename never re-homes the records that reference it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RepositoryId(String);

impl RepositoryId {
    /// Mint a fresh random identifier.
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

/// Whether a repository is serving or has been administratively turned off. Disable is reversible and
/// keeps the record, so a repository can be re-enabled without recreating it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryState {
    Enabled,
    Disabled,
}

/// The durable definition of one repository.
///
/// `route`, `ecosystem`, and `id` are fixed for the record's life; a display-name change or a
/// definition edit keeps all three so references stay valid. `definition` is opaque JSON the store
/// never reads.
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

/// The fields a create supplies; the store assigns the id, version, state, and timestamps.
#[derive(Debug)]
pub struct NewRepository {
    pub route: String,
    pub display_name: String,
    pub ecosystem: String,
    pub definition: serde_json::Value,
    pub created_by: UserId,
}

/// The fields an update replaces. `route` and `ecosystem` are immutable, so they are absent here.
#[derive(Debug)]
pub struct RepositoryUpdate {
    pub display_name: String,
    pub definition: serde_json::Value,
}

/// A rejected repository field: a create or update validates the definition before it opens a
/// transaction, so an invalid request never touches the store.
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

/// A rejected repository create.
#[derive(Debug, thiserror::Error)]
pub enum CreateRepositoryError {
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error(transparent)]
    Field(#[from] RepositoryFieldError),
    #[error("route {route} is already taken by another repository")]
    DuplicateRoute { route: String },
}

/// A rejected repository update. `VersionConflict` reports the winning version so a caller can refetch
/// and retry.
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

/// A rejected enable or disable transition.
#[derive(Debug, thiserror::Error)]
pub enum RepositoryStateError {
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error("no repository holds this identifier")]
    NotFound,
    #[error("repository is at version {current}, not the expected version")]
    VersionConflict { current: u64 },
}

/// A bounded, stable query over repository records in identifier order.
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

/// One bounded page of repository records in identifier order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepositoryPage {
    pub repositories: Vec<RepositoryRecord>,
    pub next_cursor: Option<String>,
}

/// A rejected repository list.
#[derive(Debug, thiserror::Error)]
pub enum RepositoryQueryError {
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error("limit must be between 1 and {MAX_QUERY_LIMIT}")]
    InvalidLimit,
}

/// One repository a configuration source wants persisted, matched to a record by its route.
///
/// A migration builds one of these per configured repository. The route is the match key: a route
/// already backed by a record keeps that record's identifier and version lineage, so a repository
/// carried over from TOML is never re-homed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredRepository {
    pub route: String,
    pub display_name: String,
    pub ecosystem: String,
    pub definition: serde_json::Value,
}

/// What [`reconcile_repositories`](MetaStore::reconcile_repositories) did for one desired repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileAction {
    Created,
    Updated,
    Unchanged,
}

/// The record a reconcile settled on for one route, and how it got there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconciledRepository {
    pub record: RepositoryRecord,
    pub action: ReconcileAction,
}

/// A rejected reconcile. The whole batch commits or none of it does, so any of these leaves every
/// record untouched.
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
    /// Create a repository at version 1, rejecting a route another repository already holds.
    ///
    /// # Errors
    /// Returns [`RepositoryFieldError`] for an empty or oversized field, [`CreateRepositoryError::DuplicateRoute`]
    /// when the route is taken, or a store error when the record cannot be encoded or committed.
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

    /// Inspect one repository by its identifier.
    ///
    /// # Errors
    /// Returns a store error when the record cannot be read or decoded.
    pub fn repository(&self, id: &RepositoryId) -> Result<Option<RepositoryRecord>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(REPOSITORY)?;
        Ok(table
            .get(id.as_str())?
            .map(|value| serde_json::from_slice(value.value()))
            .transpose()?)
    }

    /// Inspect the repository serving `route`, resolved through the route index.
    ///
    /// # Errors
    /// Returns a store error when the record cannot be read or decoded.
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

    /// Replace a repository's display name and definition, requiring `expected_version` to match, and
    /// commit the next version. The identifier, route, and ecosystem are preserved.
    ///
    /// # Errors
    /// Returns [`RepositoryFieldError`] for an invalid display name, [`UpdateRepositoryError::NotFound`]
    /// when no record holds the identifier, [`UpdateRepositoryError::VersionConflict`] when the
    /// precondition is stale, or a store error when the record cannot be read, encoded, or committed.
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

    /// Enable or disable a repository, requiring `expected_version` to match. A transition commits the
    /// next version; a request for the state a repository already holds is an idempotent no-op that
    /// returns the record unchanged.
    ///
    /// # Errors
    /// Returns [`RepositoryStateError::NotFound`] when no record holds the identifier,
    /// [`RepositoryStateError::VersionConflict`] when the precondition is stale, or a store error when
    /// the record cannot be read, encoded, or committed.
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

    /// List repositories in identifier order with an exclusive cursor, optionally filtered by state.
    ///
    /// # Errors
    /// Returns [`RepositoryQueryError::InvalidLimit`] for an out-of-range limit, or a store error when
    /// rows cannot be read or decoded.
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

    /// Reconcile the store against a configuration source, minting a stable identifier for each new
    /// route and preserving the identifier of every route already backed by a record.
    ///
    /// A route absent from the store is created at version 1; a route whose display name or definition
    /// changed commits its next version; an unchanged route keeps its version. Routes in the store but
    /// absent from `desired` are left untouched, so a stable identifier outlives a configuration edit
    /// that stops mentioning it. Ecosystem is immutable: a route whose configured ecosystem no longer
    /// matches its record is rejected rather than re-homed. The whole batch commits in one transaction,
    /// so any rejection leaves every record untouched.
    ///
    /// # Errors
    /// Returns [`ReconcileRepositoryError::DuplicateRoute`] when `desired` names one route twice,
    /// [`ReconcileRepositoryError::EcosystemChanged`] when a route's ecosystem no longer matches its
    /// record, [`RepositoryFieldError`] for an invalid field, or a store error when the batch cannot be
    /// read, encoded, or committed.
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
