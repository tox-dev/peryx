use redb::{ReadableDatabase as _, ReadableTable as _};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    MetaError, MetaStore, QUOTA_BLOB, QUOTA_PENDING, QUOTA_PROJECT, QUOTA_RESERVATION, QUOTA_USAGE, QUOTA_VERSION,
};

const MAX_IDENTITY_BYTES: usize = 512;

/// The storage lifecycle that owns an allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountingClass {
    Hosted,
    Cached,
    Generated,
    Trash,
}

/// The committed and pending portions of one counter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaValue {
    pub committed: u64,
    pub reserved: u64,
}

impl QuotaValue {
    #[must_use]
    const fn total(self) -> u64 {
        self.committed.saturating_add(self.reserved)
    }
}

/// Repository quota use. File bytes count logical allocations; accounted bytes charge one digest
/// once within the repository.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaUsage {
    pub file_bytes: QuotaValue,
    pub accounted_bytes: QuotaValue,
    pub projects: QuotaValue,
}

/// Version use for one repository project.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaProjectUsage {
    pub versions: QuotaValue,
}

/// Limits for reservation admission.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QuotaLimits {
    pub max_file_bytes: Option<u64>,
    pub max_accounted_bytes: Option<u64>,
    pub max_projects: Option<u64>,
    pub max_versions_per_project: Option<u64>,
    pub audit: bool,
}

/// The counter that crossed its configured limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaLimit {
    FileBytes,
    AccountedBytes,
    Projects,
    VersionsPerProject,
}

/// Capacity one writer wants to reserve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewQuotaReservation<'a> {
    pub repository: &'a str,
    pub project: Option<&'a str>,
    pub version: Option<&'a str>,
    pub digest: &'a str,
    pub bytes: u64,
    pub class: AccountingClass,
    pub created_at_unix: i64,
}

/// Whether an allocation guards an in-progress write or committed content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaReservationState {
    Reserved,
    Committed,
}

/// A durable allocation. Peryx retains committed records so deletion can release their counters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaReservationRecord {
    pub id: Uuid,
    pub repository: String,
    pub project: Option<String>,
    pub version: Option<String>,
    pub digest: String,
    pub bytes: u64,
    pub class: AccountingClass,
    pub state: QuotaReservationState,
    pub created_at_unix: i64,
    pub violations: Vec<QuotaLimit>,
}

/// Progress from one bounded restart repair pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QuotaRepairReport {
    pub released: usize,
    pub remaining: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum QuotaError {
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error("{field} must not be empty")]
    Empty { field: &'static str },
    #[error("{field} exceeds {max} bytes")]
    FieldTooLong { field: &'static str, max: usize },
    #[error("version requires a project")]
    VersionWithoutProject,
    #[error("digest {digest:?} was already accounted with {actual} bytes, not {requested}")]
    DigestSize {
        digest: String,
        actual: u64,
        requested: u64,
    },
    #[error("quota counter overflow")]
    CounterOverflow,
    #[error("quota reservation {id} is missing or already committed")]
    ReservationUnavailable { id: Uuid },
    #[error("quota exceeded: {violations:?}")]
    Exceeded { violations: Vec<QuotaLimit> },
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
struct References {
    committed: u64,
    reserved: u64,
}

impl References {
    const fn total(self) -> u64 {
        self.committed.saturating_add(self.reserved)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ProjectUsage {
    references: References,
    versions: QuotaValue,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct BlobUsage {
    bytes: u64,
    references: References,
}

struct ReservationRows {
    usage: QuotaUsage,
    project: ProjectUsage,
    version: References,
    blob: BlobUsage,
    project_key: Option<String>,
    version_key: Option<String>,
    blob_key: String,
}

impl MetaStore {
    /// Reserve counters after checking limits in the same write transaction.
    ///
    /// # Errors
    /// Returns a validation, limit, overflow, decode, or store error without changing counters.
    pub fn reserve_quota(
        &self,
        request: NewQuotaReservation<'_>,
        limits: QuotaLimits,
    ) -> Result<QuotaReservationRecord, QuotaError> {
        validate_request(&request)?;
        let txn = self.db.begin_write().map_err(MetaError::from)?;
        let mut rows = ReservationRows::read(&txn, &request)?;
        let violations = rows.reserve(request.bytes, limits)?;

        let reservation = QuotaReservationRecord {
            id: Uuid::new_v4(),
            repository: request.repository.to_owned(),
            project: request.project.map(str::to_owned),
            version: request.version.map(str::to_owned),
            digest: request.digest.to_owned(),
            bytes: request.bytes,
            class: request.class,
            state: QuotaReservationState::Reserved,
            created_at_unix: request.created_at_unix,
            violations,
        };
        rows.write(&txn, request.repository)?;
        write_record(&txn, QUOTA_RESERVATION, &reservation.id.to_string(), &reservation)?;
        txn.open_table(QUOTA_PENDING)
            .map_err(MetaError::from)?
            .insert(reservation.id.as_u128(), 0)
            .map_err(MetaError::from)?;
        txn.commit().map_err(MetaError::from)?;
        Ok(reservation)
    }

    /// Move a reservation from pending to committed counters.
    ///
    /// # Errors
    /// Returns a decode, overflow, or store error. An unknown or already committed ID returns
    /// `Ok(false)`.
    pub fn commit_quota_reservation(&self, id: Uuid) -> Result<bool, QuotaError> {
        let txn = self.db.begin_write().map_err(MetaError::from)?;
        let committed = commit_reservation(&txn, id)?;
        txn.commit().map_err(MetaError::from)?;
        Ok(committed)
    }

    /// Commit driver metadata and its reserved quota allocation together.
    ///
    /// # Errors
    /// Returns the body's error, [`QuotaError::ReservationUnavailable`], or a store error. Peryx
    /// rolls back driver and quota rows when either step fails.
    pub fn commit_driver_txn_with_quota<T, E>(
        &self,
        id: Uuid,
        body: impl FnOnce(&mut super::DriverTxn) -> Result<(T, Vec<Vec<u8>>), E>,
    ) -> Result<T, E>
    where
        E: From<MetaError> + From<QuotaError>,
    {
        self.commit_driver_txn_at(
            None,
            None,
            |txn| {
                commit_reservation(txn, id)?
                    .then_some(())
                    .ok_or_else(|| QuotaError::ReservationUnavailable { id }.into())
            },
            body,
        )
    }

    /// Release a pending or committed allocation. A second release returns `false` and changes no counters.
    ///
    /// # Errors
    /// Returns a decode or store error without partially changing counters.
    pub fn release_quota_reservation(&self, id: Uuid) -> Result<bool, QuotaError> {
        let txn = self.db.begin_write().map_err(MetaError::from)?;
        let released = release(&txn, id)?;
        txn.commit().map_err(MetaError::from)?;
        Ok(released)
    }

    /// Release at most `limit` abandoned pending reservations after restart.
    ///
    /// # Errors
    /// Returns a decode or store error without partially changing counters.
    pub fn repair_abandoned_quota_reservations(&self, limit: usize) -> Result<QuotaRepairReport, QuotaError> {
        if limit == 0 {
            return Ok(QuotaRepairReport::default());
        }
        let txn = self.db.begin_write().map_err(MetaError::from)?;
        let (ids, remaining) = {
            let table = txn.open_table(QUOTA_PENDING).map_err(MetaError::from)?;
            let mut entries = table.iter().map_err(MetaError::from)?;
            let ids = entries
                .by_ref()
                .take(limit)
                .map(|entry| {
                    entry
                        .map(|(id, _)| Uuid::from_u128(id.value()))
                        .map_err(MetaError::from)
                })
                .collect::<Result<Vec<_>, _>>()?;
            (ids, entries.next().transpose().map_err(MetaError::from)?.is_some())
        };
        for id in &ids {
            release(&txn, *id)?;
        }
        txn.commit().map_err(MetaError::from)?;
        Ok(QuotaRepairReport {
            released: ids.len(),
            remaining,
        })
    }

    /// Read repository quota counters.
    ///
    /// # Errors
    /// Returns a decode or store error.
    pub fn quota_usage(&self, repository: &str) -> Result<QuotaUsage, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(QUOTA_USAGE)?;
        Ok(read_record(&table, repository)?.unwrap_or_default())
    }

    /// Read one project's version counters.
    ///
    /// # Errors
    /// Returns a decode or store error.
    pub fn quota_project_usage(&self, repository: &str, project: &str) -> Result<QuotaProjectUsage, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(QUOTA_PROJECT)?;
        let project: ProjectUsage = read_record(&table, &identity_key((repository, project))?)?.unwrap_or_default();
        Ok(QuotaProjectUsage {
            versions: project.versions,
        })
    }

    /// Read one allocation by its stable ID.
    ///
    /// # Errors
    /// Returns a decode or store error.
    pub fn quota_reservation(&self, id: Uuid) -> Result<Option<QuotaReservationRecord>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(QUOTA_RESERVATION)?;
        read_record(&table, &id.to_string())
    }
}

impl ReservationRows {
    fn read(txn: &redb::WriteTransaction, request: &NewQuotaReservation<'_>) -> Result<Self, QuotaError> {
        let usage = {
            let table = txn.open_table(QUOTA_USAGE).map_err(MetaError::from)?;
            read_record(&table, request.repository)?
        }
        .unwrap_or_default();
        let project_key = request
            .project
            .map(|project| identity_key((request.repository, project)))
            .transpose()
            .map_err(MetaError::from)?;
        let version_key = request
            .project
            .zip(request.version)
            .map(|(project, version)| identity_key((request.repository, project, version)))
            .transpose()
            .map_err(MetaError::from)?;
        let blob_key = identity_key((request.repository, request.digest)).map_err(MetaError::from)?;
        let project = if let Some(key) = project_key.as_deref() {
            let table = txn.open_table(QUOTA_PROJECT).map_err(MetaError::from)?;
            read_record(&table, key)?.unwrap_or_default()
        } else {
            ProjectUsage::default()
        };
        let version = if let Some(key) = version_key.as_deref() {
            let table = txn.open_table(QUOTA_VERSION).map_err(MetaError::from)?;
            read_record(&table, key)?.unwrap_or_default()
        } else {
            References::default()
        };
        let mut blob: BlobUsage = {
            let table = txn.open_table(QUOTA_BLOB).map_err(MetaError::from)?;
            read_record(&table, &blob_key)?
        }
        .unwrap_or_default();
        if blob.references.total() > 0 && blob.bytes != request.bytes {
            return Err(QuotaError::DigestSize {
                digest: request.digest.to_owned(),
                actual: blob.bytes,
                requested: request.bytes,
            });
        }
        blob.bytes = request.bytes;
        Ok(Self {
            usage,
            project,
            version,
            blob,
            project_key,
            version_key,
            blob_key,
        })
    }

    fn reserve(&mut self, bytes: u64, limits: QuotaLimits) -> Result<Vec<QuotaLimit>, QuotaError> {
        let adds_project = self.project_key.is_some() && self.project.references.total() == 0;
        let adds_version = self.version_key.is_some() && self.version.total() == 0;
        let adds_accounted_bytes = self.blob.references.total() == 0;
        ensure_total_add(self.usage.file_bytes, bytes)?;
        if adds_accounted_bytes {
            ensure_total_add(self.usage.accounted_bytes, bytes)?;
        }
        if adds_project {
            ensure_total_add(self.usage.projects, 1)?;
        }
        if adds_version {
            ensure_total_add(self.project.versions, 1)?;
        }
        ensure_references_add(self.blob.references)?;
        if self.project_key.is_some() {
            ensure_references_add(self.project.references)?;
        }
        if self.version_key.is_some() {
            ensure_references_add(self.version)?;
        }
        let violations = limit_violations(
            &self.usage,
            &self.project,
            bytes,
            adds_accounted_bytes,
            adds_project,
            adds_version,
            limits,
        );
        if !limits.audit && !violations.is_empty() {
            return Err(QuotaError::Exceeded { violations });
        }

        checked_add(&mut self.usage.file_bytes.reserved, bytes)?;
        if adds_accounted_bytes {
            checked_add(&mut self.usage.accounted_bytes.reserved, bytes)?;
        }
        checked_add(&mut self.blob.references.reserved, 1)?;
        if adds_project {
            checked_add(&mut self.usage.projects.reserved, 1)?;
        }
        if self.project_key.is_some() {
            checked_add(&mut self.project.references.reserved, 1)?;
        }
        if adds_version {
            checked_add(&mut self.project.versions.reserved, 1)?;
        }
        if self.version_key.is_some() {
            checked_add(&mut self.version.reserved, 1)?;
        }
        Ok(violations)
    }

    fn write(self, txn: &redb::WriteTransaction, repository: &str) -> Result<(), QuotaError> {
        write_record(txn, QUOTA_USAGE, repository, &self.usage)?;
        write_record(txn, QUOTA_BLOB, &self.blob_key, &self.blob)?;
        if let Some(key) = self.project_key {
            write_record(txn, QUOTA_PROJECT, &key, &self.project)?;
        }
        if let Some(key) = self.version_key {
            write_record(txn, QUOTA_VERSION, &key, &self.version)?;
        }
        Ok(())
    }
}

fn limit_violations(
    usage: &QuotaUsage,
    project: &ProjectUsage,
    bytes: u64,
    adds_accounted_bytes: bool,
    adds_project: bool,
    adds_version: bool,
    limits: QuotaLimits,
) -> Vec<QuotaLimit> {
    let mut violations = Vec::new();
    if limits.max_file_bytes.is_some_and(|limit| bytes > limit) {
        violations.push(QuotaLimit::FileBytes);
    }
    if adds_accounted_bytes
        && limits.max_accounted_bytes.is_some_and(|limit| {
            usage
                .accounted_bytes
                .total()
                .checked_add(bytes)
                .is_none_or(|total| total > limit)
        })
    {
        violations.push(QuotaLimit::AccountedBytes);
    }
    if adds_project
        && limits
            .max_projects
            .is_some_and(|limit| usage.projects.total().checked_add(1).is_none_or(|total| total > limit))
    {
        violations.push(QuotaLimit::Projects);
    }
    if adds_version
        && limits.max_versions_per_project.is_some_and(|limit| {
            project
                .versions
                .total()
                .checked_add(1)
                .is_none_or(|total| total > limit)
        })
    {
        violations.push(QuotaLimit::VersionsPerProject);
    }
    violations
}

fn transition(
    txn: &redb::WriteTransaction,
    reservation: &QuotaReservationRecord,
    commit: bool,
) -> Result<(), QuotaError> {
    let mut usage: QuotaUsage = {
        let table = txn.open_table(QUOTA_USAGE).map_err(MetaError::from)?;
        read_record(&table, &reservation.repository)?
    }
    .unwrap_or_default();
    let blob_key = identity_key((&reservation.repository, &reservation.digest)).map_err(MetaError::from)?;
    let mut blob: BlobUsage = {
        let table = txn.open_table(QUOTA_BLOB).map_err(MetaError::from)?;
        read_record(&table, &blob_key)?
    }
    .unwrap_or_default();
    if commit {
        move_value(&mut usage.file_bytes, reservation.bytes)?;
        if blob.references.committed == 0 {
            move_value(&mut usage.accounted_bytes, reservation.bytes)?;
        }
        blob.references.reserved -= 1;
        checked_add(&mut blob.references.committed, 1)?;
    } else {
        let (state, bytes) = (reservation.state, reservation.bytes);
        subtract_value(&mut usage.file_bytes, state, bytes);
        subtract_reference(&mut blob.references, state);
        rebalance_or_remove(&mut usage.accounted_bytes, blob.references, state, bytes)?;
    }
    write_record(txn, QUOTA_USAGE, &reservation.repository, &usage)?;
    if blob.references.total() == 0 {
        txn.open_table(QUOTA_BLOB)
            .map_err(MetaError::from)?
            .remove(blob_key.as_str())
            .map_err(MetaError::from)?;
    } else {
        write_record(txn, QUOTA_BLOB, &blob_key, &blob)?;
    }
    transition_project(txn, reservation, commit)
}

fn transition_project(
    txn: &redb::WriteTransaction,
    reservation: &QuotaReservationRecord,
    commit: bool,
) -> Result<(), QuotaError> {
    let Some(project_name) = &reservation.project else {
        return Ok(());
    };
    let key = identity_key((&reservation.repository, project_name)).map_err(MetaError::from)?;
    let mut project: ProjectUsage = {
        let table = txn.open_table(QUOTA_PROJECT).map_err(MetaError::from)?;
        read_record(&table, &key)?
    }
    .unwrap_or_default();
    let mut usage: QuotaUsage = {
        let table = txn.open_table(QUOTA_USAGE).map_err(MetaError::from)?;
        read_record(&table, &reservation.repository)?
    }
    .unwrap_or_default();
    if commit {
        if project.references.committed == 0 {
            move_value(&mut usage.projects, 1)?;
        }
        project.references.reserved -= 1;
        checked_add(&mut project.references.committed, 1)?;
    } else {
        subtract_reference(&mut project.references, reservation.state);
        rebalance_or_remove(&mut usage.projects, project.references, reservation.state, 1)?;
    }
    transition_version(txn, reservation, commit, &mut project)?;
    write_record(txn, QUOTA_USAGE, &reservation.repository, &usage)?;
    if project.references.total() == 0 {
        txn.open_table(QUOTA_PROJECT)
            .map_err(MetaError::from)?
            .remove(key.as_str())
            .map_err(MetaError::from)?;
    } else {
        write_record(txn, QUOTA_PROJECT, &key, &project)?;
    }
    Ok(())
}

fn transition_version(
    txn: &redb::WriteTransaction,
    reservation: &QuotaReservationRecord,
    commit: bool,
    project: &mut ProjectUsage,
) -> Result<(), QuotaError> {
    let (Some(project_name), Some(version_name)) = (&reservation.project, &reservation.version) else {
        return Ok(());
    };
    let key = identity_key((&reservation.repository, project_name, version_name)).map_err(MetaError::from)?;
    let mut version: References = {
        let table = txn.open_table(QUOTA_VERSION).map_err(MetaError::from)?;
        read_record(&table, &key)?
    }
    .unwrap_or_default();
    if commit {
        if version.committed == 0 {
            move_value(&mut project.versions, 1)?;
        }
        version.reserved -= 1;
        checked_add(&mut version.committed, 1)?;
    } else {
        subtract_reference(&mut version, reservation.state);
        rebalance_or_remove(&mut project.versions, version, reservation.state, 1)?;
    }
    if version.total() == 0 {
        txn.open_table(QUOTA_VERSION)
            .map_err(MetaError::from)?
            .remove(key.as_str())
            .map_err(MetaError::from)?;
    } else {
        write_record(txn, QUOTA_VERSION, &key, &version)?;
    }
    Ok(())
}

fn release(txn: &redb::WriteTransaction, id: Uuid) -> Result<bool, QuotaError> {
    let key = id.to_string();
    let Some(reservation): Option<QuotaReservationRecord> = ({
        let table = txn.open_table(QUOTA_RESERVATION).map_err(MetaError::from)?;
        read_record(&table, &key)?
    }) else {
        return Ok(false);
    };
    transition(txn, &reservation, false)?;
    txn.open_table(QUOTA_RESERVATION)
        .map_err(MetaError::from)?
        .remove(key.as_str())
        .map_err(MetaError::from)?;
    txn.open_table(QUOTA_PENDING)
        .map_err(MetaError::from)?
        .remove(id.as_u128())
        .map_err(MetaError::from)?;
    Ok(true)
}

fn commit_reservation(txn: &redb::WriteTransaction, id: Uuid) -> Result<bool, QuotaError> {
    let key = id.to_string();
    let Some(mut reservation): Option<QuotaReservationRecord> = ({
        let table = txn.open_table(QUOTA_RESERVATION).map_err(MetaError::from)?;
        read_record(&table, &key)?
    }) else {
        return Ok(false);
    };
    if reservation.state == QuotaReservationState::Committed {
        return Ok(false);
    }
    transition(txn, &reservation, true)?;
    reservation.state = QuotaReservationState::Committed;
    write_record(txn, QUOTA_RESERVATION, &key, &reservation)?;
    txn.open_table(QUOTA_PENDING)
        .map_err(MetaError::from)?
        .remove(id.as_u128())
        .map_err(MetaError::from)?;
    Ok(true)
}

fn validate_request(request: &NewQuotaReservation<'_>) -> Result<(), QuotaError> {
    for (field, value) in [
        ("repository", Some(request.repository)),
        ("project", request.project),
        ("version", request.version),
        ("digest", Some(request.digest)),
    ] {
        if value.is_some_and(str::is_empty) {
            return Err(QuotaError::Empty { field });
        }
        if value.is_some_and(|value| value.len() > MAX_IDENTITY_BYTES) {
            return Err(QuotaError::FieldTooLong {
                field,
                max: MAX_IDENTITY_BYTES,
            });
        }
    }
    if request.version.is_some() && request.project.is_none() {
        return Err(QuotaError::VersionWithoutProject);
    }
    Ok(())
}

fn identity_key(value: impl Serialize) -> Result<String, serde_json::Error> {
    serde_json::to_string(&value)
}

fn checked_add(value: &mut u64, amount: u64) -> Result<(), QuotaError> {
    *value = value.checked_add(amount).ok_or(QuotaError::CounterOverflow)?;
    Ok(())
}

fn ensure_total_add(value: QuotaValue, amount: u64) -> Result<(), QuotaError> {
    value
        .committed
        .checked_add(value.reserved)
        .and_then(|total| total.checked_add(amount))
        .ok_or(QuotaError::CounterOverflow)?;
    Ok(())
}

fn ensure_references_add(references: References) -> Result<(), QuotaError> {
    references
        .committed
        .checked_add(references.reserved)
        .and_then(|total| total.checked_add(1))
        .ok_or(QuotaError::CounterOverflow)?;
    Ok(())
}

fn move_value(value: &mut QuotaValue, amount: u64) -> Result<(), QuotaError> {
    value.reserved -= amount;
    checked_add(&mut value.committed, amount)
}

fn subtract_value(value: &mut QuotaValue, state: QuotaReservationState, amount: u64) {
    if state == QuotaReservationState::Committed {
        value.committed -= amount;
    } else {
        value.reserved -= amount;
    }
}

fn subtract_reference(references: &mut References, state: QuotaReservationState) {
    if state == QuotaReservationState::Committed {
        references.committed -= 1;
    } else {
        references.reserved -= 1;
    }
}

fn rebalance_or_remove(
    value: &mut QuotaValue,
    references: References,
    released: QuotaReservationState,
    amount: u64,
) -> Result<(), QuotaError> {
    if references.total() == 0 {
        subtract_value(value, released, amount);
    } else if released == QuotaReservationState::Committed && references.committed == 0 {
        value.committed -= amount;
        checked_add(&mut value.reserved, amount)?;
    }
    Ok(())
}

fn read_record<T: for<'de> Deserialize<'de>>(
    table: &impl redb::ReadableTable<&'static str, &'static [u8]>,
    key: &str,
) -> Result<Option<T>, MetaError> {
    Ok(table
        .get(key)?
        .map(|value| serde_json::from_slice(value.value()))
        .transpose()?)
}

fn write_record<T: Serialize>(
    txn: &redb::WriteTransaction,
    definition: redb::TableDefinition<'static, &'static str, &'static [u8]>,
    key: &str,
    value: &T,
) -> Result<(), MetaError> {
    let encoded = serde_json::to_vec(value)?;
    txn.open_table(definition)
        .map_err(MetaError::from)?
        .insert(key, encoded.as_slice())?;
    Ok(())
}
