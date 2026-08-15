use redb::ReadableTable as _;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    MetaError, MetaStore, QUOTA_ALLOCATION, QUOTA_BLOB, QUOTA_GROUP, QUOTA_PENDING, QUOTA_RESERVATION, QUOTA_RESOURCE,
    QUOTA_USAGE,
};

const MAX_IDENTITY_BYTES: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountingClass {
    Hosted,
    Cached,
    Generated,
    Trash,
}

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

/// Artifact bytes count logical allocations; accounted bytes charge each digest once per repository.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaUsage {
    pub artifact_bytes: QuotaValue,
    pub accounted_bytes: QuotaValue,
    pub resources: QuotaValue,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaResourceUsage {
    pub artifact_bytes: QuotaValue,
    pub groups: QuotaValue,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QuotaLimits {
    pub max_artifact_bytes: Option<u64>,
    pub max_accounted_bytes: Option<u64>,
    pub max_resources: Option<u64>,
    pub max_groups_per_resource: Option<u64>,
    pub audit: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaLimit {
    ArtifactBytes,
    AccountedBytes,
    Resources,
    GroupsPerResource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewQuotaReservation<'a> {
    pub repository: &'a str,
    pub resource: Option<&'a str>,
    pub group: Option<&'a str>,
    pub digest: &'a str,
    pub bytes: u64,
    pub class: AccountingClass,
    pub created_at_unix: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaReservationState {
    Reserved,
    Committed,
}

/// Committed records remain until deletion releases their counters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaReservationRecord {
    pub id: Uuid,
    pub repository: String,
    pub resource: Option<String>,
    pub group: Option<String>,
    pub digest: String,
    pub bytes: u64,
    pub class: AccountingClass,
    pub state: QuotaReservationState,
    pub created_at_unix: i64,
    pub violations: Vec<QuotaLimit>,
    pub resource_artifact_bytes: bool,
}

/// Lets driver-object deletion locate and release its committed counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaAllocation<'a> {
    pub repository: &'a str,
    pub resource: Option<&'a str>,
    pub group: Option<&'a str>,
    pub digest: &'a str,
}

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
    #[error("group requires a resource")]
    GroupWithoutResource,
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
    #[error("resource quota exceeded at {total} bytes")]
    ResourceExceeded { total: u64 },
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
struct ResourceUsage {
    references: References,
    #[serde(default)]
    artifact_bytes: QuotaValue,
    groups: QuotaValue,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct BlobUsage {
    bytes: u64,
    references: References,
}

struct ReservationRows {
    usage: QuotaUsage,
    resource: ResourceUsage,
    group: References,
    blob: BlobUsage,
    resource_key: Option<String>,
    group_key: Option<String>,
    blob_key: String,
}

#[derive(Clone, Copy)]
struct ReservationAdds {
    accounted_bytes: bool,
    resource: bool,
    group: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReleaseScope {
    Any,
    Pending,
}

impl MetaStore {
    /// Checks limits and reserves counters atomically.
    ///
    /// # Errors
    /// Returns a validation, limit, overflow, decode, or store error without changing counters.
    pub fn reserve_quota(
        &self,
        request: NewQuotaReservation<'_>,
        limits: QuotaLimits,
    ) -> Result<QuotaReservationRecord, QuotaError> {
        self.reserve_quota_inner(request, limits, None)
    }

    /// Checks the resource limit and reserves logical bytes atomically.
    ///
    /// # Errors
    /// Returns a validation, limit, overflow, decode, or store error without changing counters.
    pub fn reserve_resource_quota(
        &self,
        request: NewQuotaReservation<'_>,
        max_resource_artifact_bytes: u64,
        audit: bool,
    ) -> Result<QuotaReservationRecord, QuotaError> {
        if request.resource.is_none() {
            return Err(QuotaError::Empty { field: "resource" });
        }
        self.reserve_quota_inner(
            request,
            QuotaLimits {
                audit,
                ..QuotaLimits::default()
            },
            Some(max_resource_artifact_bytes),
        )
    }

    fn reserve_quota_inner(
        &self,
        request: NewQuotaReservation<'_>,
        limits: QuotaLimits,
        max_resource_artifact_bytes: Option<u64>,
    ) -> Result<QuotaReservationRecord, QuotaError> {
        validate_request(&request)?;
        let txn = self.db.begin_write().map_err(MetaError::from)?;
        let mut rows = ReservationRows::read(&txn, &request)?;

        let reservation = QuotaReservationRecord {
            id: Uuid::new_v4(),
            repository: request.repository.to_owned(),
            resource: request.resource.map(str::to_owned),
            group: request.group.map(str::to_owned),
            digest: request.digest.to_owned(),
            bytes: request.bytes,
            class: request.class,
            state: QuotaReservationState::Reserved,
            created_at_unix: request.created_at_unix,
            violations: rows.reserve(request.bytes, limits, max_resource_artifact_bytes)?,
            resource_artifact_bytes: max_resource_artifact_bytes.is_some(),
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

    /// # Errors
    /// Returns a decode, overflow, or store error. An unknown or already committed ID returns
    /// `Ok(false)`.
    pub fn commit_quota_reservation(&self, id: Uuid) -> Result<bool, QuotaError> {
        let txn = self.db.begin_write().map_err(MetaError::from)?;
        let committed = commit_reservation(&txn, id)?;
        txn.commit().map_err(MetaError::from)?;
        Ok(committed)
    }

    /// Commits driver metadata and its quota allocation atomically.
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
            true,
            |txn, _| {
                commit_reservation(txn, id)?
                    .then_some(())
                    .ok_or_else(|| QuotaError::ReservationUnavailable { id }.into())
            },
            body,
        )
    }

    /// Commits driver metadata and atomically commits or releases quota according to `body`.
    ///
    /// # Errors
    /// Returns the body's error, [`QuotaError::ReservationUnavailable`], or a store error. Peryx
    /// rolls back driver and quota rows when either step fails.
    pub fn commit_driver_txn_with_quota_if<T, E>(
        &self,
        id: Uuid,
        commit: impl FnOnce(&T) -> bool,
        body: impl FnOnce(&mut super::DriverTxn) -> Result<(T, Vec<Vec<u8>>), E>,
    ) -> Result<T, E>
    where
        E: From<MetaError> + From<QuotaError>,
    {
        self.commit_driver_txn_at(
            None,
            None,
            true,
            |txn, value| {
                (if commit(value) {
                    commit_reservation(txn, id)?
                } else {
                    release(txn, id, ReleaseScope::Pending)?
                })
                .then_some(())
                .ok_or_else(|| QuotaError::ReservationUnavailable { id }.into())
            },
            body,
        )
    }

    /// # Errors
    /// Returns a quota, body, or store error without committing partial changes.
    pub fn commit_driver_txn_with_quota_if_commit<T, E>(
        &self,
        id: Uuid,
        commit: impl FnOnce(&T) -> bool,
        body: impl FnOnce(&mut super::DriverTxn) -> Result<(T, Vec<Vec<u8>>), E>,
    ) -> Result<super::DriverCommit<T>, E>
    where
        E: From<MetaError> + From<QuotaError>,
    {
        self.commit_driver_txn_at_with_commit(
            None,
            None,
            true,
            |txn, value| {
                (if commit(value) {
                    commit_reservation(txn, id)?
                } else {
                    release(txn, id, ReleaseScope::Pending)?
                })
                .then_some(())
                .ok_or_else(|| QuotaError::ReservationUnavailable { id }.into())
            },
            body,
        )
    }

    /// A repeated release returns `false` without changing counters.
    ///
    /// # Errors
    /// Returns a decode or store error without partially changing counters.
    pub fn release_quota_reservation(&self, id: Uuid) -> Result<bool, QuotaError> {
        let txn = self.db.begin_write().map_err(MetaError::from)?;
        let released = release(&txn, id, ReleaseScope::Any)?;
        txn.commit().map_err(MetaError::from)?;
        Ok(released)
    }

    /// Commits driver deletion and its accepted counter release atomically. Missing allocations release
    /// nothing.
    ///
    /// # Errors
    /// Returns the body's error or a store error. Peryx rolls back the driver rows and the counter
    /// release together when either step fails.
    pub fn commit_driver_txn_release_allocation<T, E>(
        &self,
        allocation: QuotaAllocation<'_>,
        release_allocation: impl FnOnce(&T) -> bool,
        body: impl FnOnce(&mut super::DriverTxn) -> Result<(T, Vec<Vec<u8>>), E>,
    ) -> Result<T, E>
    where
        E: From<MetaError> + From<QuotaError>,
    {
        self.commit_driver_txn_at(
            None,
            None,
            true,
            move |txn, value| {
                if release_allocation(value) {
                    release_committed_allocation(txn, &allocation)?;
                }
                Ok(())
            },
            body,
        )
    }

    /// Releases at most `limit` pending reservations abandoned by a restart.
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
            release(&txn, *id, ReleaseScope::Pending)?;
        }
        txn.commit().map_err(MetaError::from)?;
        Ok(QuotaRepairReport {
            released: ids.len(),
            remaining,
        })
    }

    /// # Errors
    /// Returns a decode or store error.
    pub fn quota_usage(&self, repository: &str) -> Result<QuotaUsage, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(QUOTA_USAGE)?;
        Ok(read_quota_usage(&table, repository)?.unwrap_or_default())
    }

    /// # Errors
    /// Returns a decode or store error.
    pub fn quota_resource_usage(&self, repository: &str, resource: &str) -> Result<QuotaResourceUsage, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(QUOTA_RESOURCE)?;
        let resource = read_resource_usage(&table, &identity_key((repository, resource))?)?.unwrap_or_default();
        Ok(QuotaResourceUsage {
            artifact_bytes: resource.artifact_bytes,
            groups: resource.groups,
        })
    }

    /// # Errors
    /// Returns a decode or store error.
    pub fn quota_reservation(&self, id: Uuid) -> Result<Option<QuotaReservationRecord>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(QUOTA_RESERVATION)?;
        read_quota_reservation(&table, &id.to_string())
    }
}

impl ReservationRows {
    fn read(txn: &redb::WriteTransaction, request: &NewQuotaReservation<'_>) -> Result<Self, QuotaError> {
        let usage = {
            let table = txn.open_table(QUOTA_USAGE).map_err(MetaError::from)?;
            read_quota_usage(&table, request.repository)?
        }
        .unwrap_or_default();
        let resource_key = request
            .resource
            .map(|resource| identity_key((request.repository, resource)))
            .transpose()
            .map_err(MetaError::from)?;
        let group_key = request
            .resource
            .zip(request.group)
            .map(|(resource, group)| identity_key((request.repository, resource, group)))
            .transpose()
            .map_err(MetaError::from)?;
        let blob_key = identity_key((request.repository, request.digest)).map_err(MetaError::from)?;
        let resource = if let Some(key) = resource_key.as_deref() {
            let table = txn.open_table(QUOTA_RESOURCE).map_err(MetaError::from)?;
            read_resource_usage(&table, key)?.unwrap_or_default()
        } else {
            ResourceUsage::default()
        };
        let group = if let Some(key) = group_key.as_deref() {
            let table = txn.open_table(QUOTA_GROUP).map_err(MetaError::from)?;
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
            resource,
            group,
            blob,
            resource_key,
            group_key,
            blob_key,
        })
    }

    fn reserve(
        &mut self,
        bytes: u64,
        limits: QuotaLimits,
        max_resource_artifact_bytes: Option<u64>,
    ) -> Result<Vec<QuotaLimit>, QuotaError> {
        let adds = ReservationAdds {
            accounted_bytes: self.blob.references.total() == 0,
            resource: self.resource_key.is_some() && self.resource.references.total() == 0,
            group: self.group_key.is_some() && self.group.total() == 0,
        };
        ensure_total_add(self.usage.artifact_bytes, bytes)?;
        if max_resource_artifact_bytes.is_some() {
            ensure_total_add(self.resource.artifact_bytes, bytes)?;
        }
        if adds.accounted_bytes {
            ensure_total_add(self.usage.accounted_bytes, bytes)?;
        }
        if adds.resource {
            ensure_total_add(self.usage.resources, 1)?;
        }
        if adds.group {
            ensure_total_add(self.resource.groups, 1)?;
        }
        ensure_references_add(self.blob.references)?;
        if self.resource_key.is_some() {
            ensure_references_add(self.resource.references)?;
        }
        if self.group_key.is_some() {
            ensure_references_add(self.group)?;
        }
        let resource_excess = max_resource_artifact_bytes.and_then(|limit| {
            let total = self.resource.artifact_bytes.total() + bytes;
            (total > limit).then_some(total)
        });
        let violations = limit_violations(self, bytes, adds, limits, resource_excess.is_some());
        if !limits.audit && !violations.is_empty() {
            if let Some(total) = resource_excess {
                return Err(QuotaError::ResourceExceeded { total });
            }
            return Err(QuotaError::Exceeded { violations });
        }

        checked_add(&mut self.usage.artifact_bytes.reserved, bytes)?;
        if max_resource_artifact_bytes.is_some() {
            checked_add(&mut self.resource.artifact_bytes.reserved, bytes)?;
        }
        if adds.accounted_bytes {
            checked_add(&mut self.usage.accounted_bytes.reserved, bytes)?;
        }
        checked_add(&mut self.blob.references.reserved, 1)?;
        if adds.resource {
            checked_add(&mut self.usage.resources.reserved, 1)?;
        }
        if self.resource_key.is_some() {
            checked_add(&mut self.resource.references.reserved, 1)?;
        }
        if adds.group {
            checked_add(&mut self.resource.groups.reserved, 1)?;
        }
        if self.group_key.is_some() {
            checked_add(&mut self.group.reserved, 1)?;
        }
        Ok(violations)
    }

    fn write(self, txn: &redb::WriteTransaction, repository: &str) -> Result<(), QuotaError> {
        write_record(txn, QUOTA_USAGE, repository, &self.usage)?;
        write_record(txn, QUOTA_BLOB, &self.blob_key, &self.blob)?;
        if let Some(key) = self.resource_key {
            write_record(txn, QUOTA_RESOURCE, &key, &self.resource)?;
        }
        if let Some(key) = self.group_key {
            write_record(txn, QUOTA_GROUP, &key, &self.group)?;
        }
        Ok(())
    }
}

fn limit_violations(
    rows: &ReservationRows,
    bytes: u64,
    adds: ReservationAdds,
    limits: QuotaLimits,
    resource_artifact_bytes_exceeded: bool,
) -> Vec<QuotaLimit> {
    let mut violations = Vec::new();
    if limits.max_artifact_bytes.is_some_and(|limit| {
        rows.usage
            .artifact_bytes
            .total()
            .checked_add(bytes)
            .is_none_or(|total| total > limit)
    }) || resource_artifact_bytes_exceeded
    {
        violations.push(QuotaLimit::ArtifactBytes);
    }
    if adds.accounted_bytes
        && limits.max_accounted_bytes.is_some_and(|limit| {
            rows.usage
                .accounted_bytes
                .total()
                .checked_add(bytes)
                .is_none_or(|total| total > limit)
        })
    {
        violations.push(QuotaLimit::AccountedBytes);
    }
    if adds.resource
        && limits.max_resources.is_some_and(|limit| {
            rows.usage
                .resources
                .total()
                .checked_add(1)
                .is_none_or(|total| total > limit)
        })
    {
        violations.push(QuotaLimit::Resources);
    }
    if adds.group
        && limits.max_groups_per_resource.is_some_and(|limit| {
            rows.resource
                .groups
                .total()
                .checked_add(1)
                .is_none_or(|total| total > limit)
        })
    {
        violations.push(QuotaLimit::GroupsPerResource);
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
        read_quota_usage(&table, &reservation.repository)?
    }
    .unwrap_or_default();
    let blob_key = identity_key((&reservation.repository, &reservation.digest)).map_err(MetaError::from)?;
    let mut blob: BlobUsage = {
        let table = txn.open_table(QUOTA_BLOB).map_err(MetaError::from)?;
        read_record(&table, &blob_key)?
    }
    .unwrap_or_default();
    if commit {
        move_value(&mut usage.artifact_bytes, reservation.bytes)?;
        if blob.references.committed == 0 {
            move_value(&mut usage.accounted_bytes, reservation.bytes)?;
        }
        blob.references.reserved -= 1;
        checked_add(&mut blob.references.committed, 1)?;
    } else {
        let (state, bytes) = (reservation.state, reservation.bytes);
        subtract_value(&mut usage.artifact_bytes, state, bytes);
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
    transition_resource(txn, reservation, commit)
}

fn transition_resource(
    txn: &redb::WriteTransaction,
    reservation: &QuotaReservationRecord,
    commit: bool,
) -> Result<(), QuotaError> {
    let Some(resource_name) = &reservation.resource else {
        return Ok(());
    };
    let key = identity_key((&reservation.repository, resource_name)).map_err(MetaError::from)?;
    let mut resource: ResourceUsage = {
        let table = txn.open_table(QUOTA_RESOURCE).map_err(MetaError::from)?;
        read_resource_usage(&table, &key)?
    }
    .unwrap_or_default();
    let mut usage: QuotaUsage = {
        let table = txn.open_table(QUOTA_USAGE).map_err(MetaError::from)?;
        read_quota_usage(&table, &reservation.repository)?
    }
    .unwrap_or_default();
    if commit {
        if resource.references.committed == 0 {
            move_value(&mut usage.resources, 1)?;
        }
        if reservation.resource_artifact_bytes {
            move_value(&mut resource.artifact_bytes, reservation.bytes)?;
        }
        resource.references.reserved -= 1;
        checked_add(&mut resource.references.committed, 1)?;
    } else {
        if reservation.resource_artifact_bytes {
            subtract_value(&mut resource.artifact_bytes, reservation.state, reservation.bytes);
        }
        subtract_reference(&mut resource.references, reservation.state);
        rebalance_or_remove(&mut usage.resources, resource.references, reservation.state, 1)?;
    }
    transition_group(txn, reservation, commit, &mut resource)?;
    write_record(txn, QUOTA_USAGE, &reservation.repository, &usage)?;
    if resource.references.total() == 0 {
        txn.open_table(QUOTA_RESOURCE)
            .map_err(MetaError::from)?
            .remove(key.as_str())
            .map_err(MetaError::from)?;
    } else {
        write_record(txn, QUOTA_RESOURCE, &key, &resource)?;
    }
    Ok(())
}

fn transition_group(
    txn: &redb::WriteTransaction,
    reservation: &QuotaReservationRecord,
    commit: bool,
    resource: &mut ResourceUsage,
) -> Result<(), QuotaError> {
    let (Some(resource_name), Some(group_name)) = (&reservation.resource, &reservation.group) else {
        return Ok(());
    };
    let key = identity_key((&reservation.repository, resource_name, group_name)).map_err(MetaError::from)?;
    let mut group: References = {
        let table = txn.open_table(QUOTA_GROUP).map_err(MetaError::from)?;
        read_record(&table, &key)?
    }
    .unwrap_or_default();
    if commit {
        if group.committed == 0 {
            move_value(&mut resource.groups, 1)?;
        }
        group.reserved -= 1;
        checked_add(&mut group.committed, 1)?;
    } else {
        subtract_reference(&mut group, reservation.state);
        rebalance_or_remove(&mut resource.groups, group, reservation.state, 1)?;
    }
    if group.total() == 0 {
        txn.open_table(QUOTA_GROUP)
            .map_err(MetaError::from)?
            .remove(key.as_str())
            .map_err(MetaError::from)?;
    } else {
        write_record(txn, QUOTA_GROUP, &key, &group)?;
    }
    Ok(())
}

fn release(txn: &redb::WriteTransaction, id: Uuid, scope: ReleaseScope) -> Result<bool, QuotaError> {
    let key = id.to_string();
    let Some(reservation): Option<QuotaReservationRecord> = ({
        let table = txn.open_table(QUOTA_RESERVATION).map_err(MetaError::from)?;
        read_quota_reservation(&table, &key)?
    }) else {
        return Ok(false);
    };
    if reservation.state == QuotaReservationState::Committed && scope == ReleaseScope::Pending {
        return Ok(false);
    }
    transition(txn, &reservation, false)?;
    if reservation.state == QuotaReservationState::Committed {
        forget_allocation(txn, &reservation)?;
    }
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
        read_quota_reservation(&table, &key)?
    }) else {
        return Ok(false);
    };
    if reservation.state == QuotaReservationState::Committed {
        return Ok(false);
    }
    transition(txn, &reservation, true)?;
    reservation.state = QuotaReservationState::Committed;
    write_record(txn, QUOTA_RESERVATION, &key, &reservation)?;
    write_record(txn, QUOTA_ALLOCATION, &allocation_key(&reservation)?, &id)?;
    txn.open_table(QUOTA_PENDING)
        .map_err(MetaError::from)?
        .remove(id.as_u128())
        .map_err(MetaError::from)?;
    Ok(true)
}

/// Removing the index entry makes repeated release a no-op.
fn release_committed_allocation(
    txn: &redb::WriteTransaction,
    allocation: &QuotaAllocation<'_>,
) -> Result<bool, QuotaError> {
    let key = identity_key((
        allocation.repository,
        allocation.resource,
        allocation.group,
        allocation.digest,
    ))
    .map_err(MetaError::from)?;
    let Some(id): Option<Uuid> = ({
        let table = txn.open_table(QUOTA_ALLOCATION).map_err(MetaError::from)?;
        read_record(&table, &key)?
    }) else {
        return Ok(false);
    };
    release(txn, id, ReleaseScope::Any)
}

/// Preserves an index entry replaced by a later duplicate.
fn forget_allocation(txn: &redb::WriteTransaction, reservation: &QuotaReservationRecord) -> Result<(), QuotaError> {
    let key = allocation_key(reservation)?;
    let mut table = txn.open_table(QUOTA_ALLOCATION).map_err(MetaError::from)?;
    if read_record::<Uuid>(&table, &key)? == Some(reservation.id) {
        table.remove(key.as_str()).map_err(MetaError::from)?;
    }
    Ok(())
}

fn allocation_key(reservation: &QuotaReservationRecord) -> Result<String, MetaError> {
    Ok(identity_key((
        reservation.repository.as_str(),
        reservation.resource.as_deref(),
        reservation.group.as_deref(),
        reservation.digest.as_str(),
    ))?)
}

fn validate_request(request: &NewQuotaReservation<'_>) -> Result<(), QuotaError> {
    for (field, value) in [
        ("repository", Some(request.repository)),
        ("resource", request.resource),
        ("group", request.group),
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
    if request.group.is_some() && request.resource.is_none() {
        return Err(QuotaError::GroupWithoutResource);
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

fn read_quota_usage(
    table: &impl redb::ReadableTable<&'static str, &'static [u8]>,
    key: &str,
) -> Result<Option<QuotaUsage>, MetaError> {
    read_record(table, key)
}

fn read_resource_usage(
    table: &impl redb::ReadableTable<&'static str, &'static [u8]>,
    key: &str,
) -> Result<Option<ResourceUsage>, MetaError> {
    read_record(table, key)
}

fn read_quota_reservation(
    table: &impl redb::ReadableTable<&'static str, &'static [u8]>,
    key: &str,
) -> Result<Option<QuotaReservationRecord>, MetaError> {
    read_record(table, key)
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
