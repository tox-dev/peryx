use std::collections::HashSet;
use std::ops::Bound;

use redb::{ReadableTable as _, TableDefinition, TableHandle as _};

use super::{
    ANALYTICS, MetaError, MetaStore, POLICY_DECISION, POLICY_DECISION_CURRENT, POLICY_DECISION_CURRENT_ID, QUOTA_GROUP,
    QUOTA_RESERVATION, QUOTA_RESOURCE, QUOTA_USAGE, open_optional_table,
};

const BATCH_SIZE: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataRecordSet {
    QuotaUsage,
    QuotaResource,
    QuotaGroup,
    QuotaReservation,
    PolicyDecisionHistory,
    PolicyDecisionCurrent,
    PolicyDecisionCurrentById,
    Analytics,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataValueKind {
    Bytes,
    Text,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LegacyMetadataSource {
    pub table: &'static str,
    pub value_kind: MetadataValueKind,
    pub target: MetadataRecordSet,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataRecord {
    pub key: String,
    pub value: Vec<u8>,
}

pub trait MetadataMigration: Send + Sync {
    fn name(&self) -> &'static str;
    fn record_sets(&self) -> &[MetadataRecordSet];

    fn legacy_sources(&self) -> &[LegacyMetadataSource];

    /// # Errors
    /// Implementations return a stable owner error when they cannot migrate a record.
    fn rewrite(&self, record_set: MetadataRecordSet, record: &MetadataRecord)
    -> Result<Option<MetadataRecord>, String>;
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MetadataMigrationReport {
    pub scanned: usize,
    pub rewritten: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum MetadataMigrationError {
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error("metadata migration {migration:?} failed for {record_set:?} record {key:?}: {message}")]
    Owner {
        migration: &'static str,
        record_set: MetadataRecordSet,
        key: String,
        message: String,
    },
    #[error("metadata migration {migration:?} produced non-UTF-8 text for {record_set:?} record {key:?}")]
    InvalidText {
        migration: &'static str,
        record_set: MetadataRecordSet,
        key: String,
    },
}

impl MetaStore {
    /// Processes current records and owner-declared legacy records in bounded transactions.
    ///
    /// # Errors
    /// A store or owner error leaves the failing batch uncommitted.
    pub fn migrate_metadata(
        &self,
        migration: &dyn MetadataMigration,
    ) -> Result<MetadataMigrationReport, MetadataMigrationError> {
        let mut report = MetadataMigrationReport::default();
        for &record_set in migration.record_sets() {
            migrate_current(self, migration, record_set, &mut report)?;
        }
        for &source in migration.legacy_sources() {
            migrate_legacy(self, migration, source, &mut report)?;
        }
        Ok(report)
    }

    /// Detects pending rewrites without requiring a writable transaction.
    ///
    /// # Errors
    /// Returns the first store or owner error without writing to the store.
    pub fn dry_run_metadata_migration(
        &self,
        migration: &dyn MetadataMigration,
    ) -> Result<MetadataMigrationReport, MetadataMigrationError> {
        let txn = self.db.begin_read().map_err(MetaError::from)?;
        let mut report = MetadataMigrationReport::default();
        for &record_set in migration.record_sets() {
            dry_run_current(&txn, migration, record_set, &mut report)?;
        }
        for &source in migration.legacy_sources() {
            dry_run_legacy(&txn, migration, source, &mut report)?;
        }
        Ok(report)
    }
}

fn dry_run_current(
    txn: &redb::ReadTransaction,
    migration: &dyn MetadataMigration,
    record_set: MetadataRecordSet,
    report: &mut MetadataMigrationReport,
) -> Result<(), MetadataMigrationError> {
    dry_run_source(
        txn,
        migration,
        target_name(record_set),
        target_value_kind(record_set),
        record_set,
        report,
    )
}

fn dry_run_legacy(
    txn: &redb::ReadTransaction,
    migration: &dyn MetadataMigration,
    source: LegacyMetadataSource,
    report: &mut MetadataMigrationReport,
) -> Result<(), MetadataMigrationError> {
    if source.table == target_name(source.target) {
        return dry_run_current(txn, migration, source.target, report);
    }
    dry_run_source(txn, migration, source.table, source.value_kind, source.target, report)
}

fn dry_run_source(
    txn: &redb::ReadTransaction,
    migration: &dyn MetadataMigration,
    table: &'static str,
    value_kind: MetadataValueKind,
    record_set: MetadataRecordSet,
    report: &mut MetadataMigrationReport,
) -> Result<(), MetadataMigrationError> {
    match value_kind {
        MetadataValueKind::Bytes => dry_run_bytes(txn, migration, table, record_set, report),
        MetadataValueKind::Text => dry_run_text(txn, migration, table, record_set, report),
    }
}

fn dry_run_bytes(
    txn: &redb::ReadTransaction,
    migration: &dyn MetadataMigration,
    table: &'static str,
    record_set: MetadataRecordSet,
    report: &mut MetadataMigrationReport,
) -> Result<(), MetadataMigrationError> {
    let Some(table) = open_optional_table(txn, TableDefinition::<&str, &[u8]>::new(table))? else {
        return Ok(());
    };
    for entry in table.iter().map_err(MetaError::from)? {
        let (key, value) = entry.map_err(MetaError::from)?;
        dry_run_record(
            migration,
            record_set,
            &MetadataRecord {
                key: key.value().to_owned(),
                value: value.value().to_vec(),
            },
            report,
        )?;
    }
    Ok(())
}

fn dry_run_text(
    txn: &redb::ReadTransaction,
    migration: &dyn MetadataMigration,
    table: &'static str,
    record_set: MetadataRecordSet,
    report: &mut MetadataMigrationReport,
) -> Result<(), MetadataMigrationError> {
    let Some(table) = open_optional_table(txn, TableDefinition::<&str, &str>::new(table))? else {
        return Ok(());
    };
    for entry in table.iter().map_err(MetaError::from)? {
        let (key, value) = entry.map_err(MetaError::from)?;
        dry_run_record(
            migration,
            record_set,
            &MetadataRecord {
                key: key.value().to_owned(),
                value: value.value().as_bytes().to_vec(),
            },
            report,
        )?;
    }
    Ok(())
}

fn dry_run_record(
    migration: &dyn MetadataMigration,
    record_set: MetadataRecordSet,
    record: &MetadataRecord,
    report: &mut MetadataMigrationReport,
) -> Result<(), MetadataMigrationError> {
    report.scanned += 1;
    report.rewritten += usize::from(rewrite(migration, record_set, record)?.is_some());
    Ok(())
}

fn migrate_current(
    store: &MetaStore,
    migration: &dyn MetadataMigration,
    record_set: MetadataRecordSet,
    report: &mut MetadataMigrationReport,
) -> Result<(), MetadataMigrationError> {
    let mut cursor = None;
    let mut created_keys = HashSet::new();
    loop {
        let txn = store.db.begin_write().map_err(MetaError::from)?;
        let records = collect_target(&txn, record_set, cursor.as_deref(), &created_keys)?;
        if records.is_empty() {
            return Ok(());
        }
        let batch_len = records.len();
        cursor = records.last().map(|record| record.key.clone());
        let (rewritten, batch_created_keys) = rewrite_current_batch(&txn, migration, record_set, records)?;
        txn.commit().map_err(MetaError::from)?;
        created_keys.extend(batch_created_keys);
        report.scanned += batch_len;
        report.rewritten += rewritten;
    }
}

fn migrate_legacy(
    store: &MetaStore,
    migration: &dyn MetadataMigration,
    source: LegacyMetadataSource,
    report: &mut MetadataMigrationReport,
) -> Result<(), MetadataMigrationError> {
    if source.table == target_name(source.target) {
        return migrate_current(store, migration, source.target, report);
    }
    let txn = store.db.begin_read().map_err(MetaError::from)?;
    if !txn
        .list_tables()
        .map_err(MetaError::from)?
        .any(|table| table.name() == source.table)
    {
        return Ok(());
    }
    drop(txn);
    let mut cursor = None;
    loop {
        let txn = store.db.begin_write().map_err(MetaError::from)?;
        let records = collect_legacy(&txn, source, cursor.as_deref())?;
        if records.is_empty() {
            if legacy_table_empty(&txn, source)? {
                delete_legacy_table(&txn, source)?;
                txn.commit().map_err(MetaError::from)?;
            }
            return Ok(());
        }
        let batch_len = records.len();
        cursor = records.last().map(|record| record.key.clone());
        let rewritten = rewrite_legacy_batch(&txn, migration, source, records)?;
        let finished = legacy_table_empty(&txn, source)?;
        if finished {
            delete_legacy_table(&txn, source)?;
        }
        txn.commit().map_err(MetaError::from)?;
        report.scanned += batch_len;
        report.rewritten += rewritten;
        if finished {
            return Ok(());
        }
    }
}

fn rewrite_current_batch(
    txn: &redb::WriteTransaction,
    migration: &dyn MetadataMigration,
    record_set: MetadataRecordSet,
    records: Vec<MetadataRecord>,
) -> Result<(usize, Vec<String>), MetadataMigrationError> {
    let mut rewritten_count = 0;
    let mut created_keys = Vec::new();
    for record in records {
        let Some(rewritten) = rewrite(migration, record_set, &record)? else {
            continue;
        };
        if rewrite_target(txn, record_set, &record, &rewritten, migration.name())? {
            created_keys.push(rewritten.key.clone());
        }
        rewritten_count += 1;
    }
    Ok((rewritten_count, created_keys))
}

fn rewrite_legacy_batch(
    txn: &redb::WriteTransaction,
    migration: &dyn MetadataMigration,
    source: LegacyMetadataSource,
    records: Vec<MetadataRecord>,
) -> Result<usize, MetadataMigrationError> {
    let mut rewritten_count = 0;
    for record in records {
        let Some(rewritten) = rewrite(migration, source.target, &record)? else {
            continue;
        };
        insert_target(txn, source.target, &record, &rewritten, migration.name())?;
        remove_legacy(txn, source, &record.key)?;
        rewritten_count += 1;
    }
    Ok(rewritten_count)
}

fn rewrite(
    migration: &dyn MetadataMigration,
    record_set: MetadataRecordSet,
    record: &MetadataRecord,
) -> Result<Option<MetadataRecord>, MetadataMigrationError> {
    migration
        .rewrite(record_set, record)
        .map_err(|message| MetadataMigrationError::Owner {
            migration: migration.name(),
            record_set,
            key: record.key.clone(),
            message,
        })
}

fn collect_target(
    txn: &redb::WriteTransaction,
    record_set: MetadataRecordSet,
    cursor: Option<&str>,
    created_keys: &HashSet<String>,
) -> Result<Vec<MetadataRecord>, MetaError> {
    match record_set {
        MetadataRecordSet::QuotaUsage => collect_bytes(txn, QUOTA_USAGE, cursor, Some(created_keys)),
        MetadataRecordSet::QuotaResource => collect_bytes(txn, QUOTA_RESOURCE, cursor, Some(created_keys)),
        MetadataRecordSet::QuotaGroup => collect_bytes(txn, QUOTA_GROUP, cursor, Some(created_keys)),
        MetadataRecordSet::QuotaReservation => collect_bytes(txn, QUOTA_RESERVATION, cursor, Some(created_keys)),
        MetadataRecordSet::PolicyDecisionHistory => collect_bytes(txn, POLICY_DECISION, cursor, Some(created_keys)),
        MetadataRecordSet::PolicyDecisionCurrent => {
            collect_text(txn, POLICY_DECISION_CURRENT, cursor, Some(created_keys))
        }
        MetadataRecordSet::PolicyDecisionCurrentById => {
            collect_bytes(txn, POLICY_DECISION_CURRENT_ID, cursor, Some(created_keys))
        }
        MetadataRecordSet::Analytics => collect_bytes(txn, ANALYTICS, cursor, Some(created_keys)),
    }
}

fn collect_legacy(
    txn: &redb::WriteTransaction,
    source: LegacyMetadataSource,
    cursor: Option<&str>,
) -> Result<Vec<MetadataRecord>, MetaError> {
    match source.value_kind {
        MetadataValueKind::Bytes => collect_bytes(txn, TableDefinition::new(source.table), cursor, None),
        MetadataValueKind::Text => collect_text(txn, TableDefinition::new(source.table), cursor, None),
    }
}

fn collect_bytes(
    txn: &redb::WriteTransaction,
    definition: TableDefinition<'static, &'static str, &'static [u8]>,
    cursor: Option<&str>,
    created_keys: Option<&HashSet<String>>,
) -> Result<Vec<MetadataRecord>, MetaError> {
    let table = txn.open_table(definition)?;
    let mut records = Vec::with_capacity(BATCH_SIZE);
    for entry in table.range::<&str>((cursor.map_or(Bound::Unbounded, Bound::Excluded), Bound::Unbounded))? {
        let (key, value) = entry?;
        if created_keys.is_some_and(|created_keys| created_keys.contains(key.value())) {
            continue;
        }
        records.push(MetadataRecord {
            key: key.value().to_owned(),
            value: value.value().to_vec(),
        });
        if records.len() == BATCH_SIZE {
            break;
        }
    }
    Ok(records)
}

fn collect_text(
    txn: &redb::WriteTransaction,
    definition: TableDefinition<'static, &'static str, &'static str>,
    cursor: Option<&str>,
    created_keys: Option<&HashSet<String>>,
) -> Result<Vec<MetadataRecord>, MetaError> {
    let table = txn.open_table(definition)?;
    let mut records = Vec::with_capacity(BATCH_SIZE);
    for entry in table.range::<&str>((cursor.map_or(Bound::Unbounded, Bound::Excluded), Bound::Unbounded))? {
        let (key, value) = entry?;
        if created_keys.is_some_and(|created_keys| created_keys.contains(key.value())) {
            continue;
        }
        records.push(MetadataRecord {
            key: key.value().to_owned(),
            value: value.value().as_bytes().to_vec(),
        });
        if records.len() == BATCH_SIZE {
            break;
        }
    }
    Ok(records)
}

fn rewrite_target(
    txn: &redb::WriteTransaction,
    record_set: MetadataRecordSet,
    original: &MetadataRecord,
    rewritten: &MetadataRecord,
    migration: &'static str,
) -> Result<bool, MetadataMigrationError> {
    match record_set {
        MetadataRecordSet::QuotaUsage => rewrite_bytes(txn, QUOTA_USAGE, original, rewritten).map_err(Into::into),
        MetadataRecordSet::QuotaResource => rewrite_bytes(txn, QUOTA_RESOURCE, original, rewritten).map_err(Into::into),
        MetadataRecordSet::QuotaGroup => rewrite_bytes(txn, QUOTA_GROUP, original, rewritten).map_err(Into::into),
        MetadataRecordSet::QuotaReservation => {
            rewrite_bytes(txn, QUOTA_RESERVATION, original, rewritten).map_err(Into::into)
        }
        MetadataRecordSet::PolicyDecisionHistory => {
            rewrite_bytes(txn, POLICY_DECISION, original, rewritten).map_err(Into::into)
        }
        MetadataRecordSet::PolicyDecisionCurrent => {
            rewrite_text(txn, POLICY_DECISION_CURRENT, original, rewritten, migration, record_set)
        }
        MetadataRecordSet::PolicyDecisionCurrentById => {
            rewrite_bytes(txn, POLICY_DECISION_CURRENT_ID, original, rewritten).map_err(Into::into)
        }
        MetadataRecordSet::Analytics => rewrite_bytes(txn, ANALYTICS, original, rewritten).map_err(Into::into),
    }
}

fn insert_target(
    txn: &redb::WriteTransaction,
    record_set: MetadataRecordSet,
    original: &MetadataRecord,
    rewritten: &MetadataRecord,
    migration: &'static str,
) -> Result<(), MetadataMigrationError> {
    match record_set {
        MetadataRecordSet::QuotaUsage => insert_bytes(txn, QUOTA_USAGE, rewritten).map_err(Into::into),
        MetadataRecordSet::QuotaResource => insert_bytes(txn, QUOTA_RESOURCE, rewritten).map_err(Into::into),
        MetadataRecordSet::QuotaGroup => insert_bytes(txn, QUOTA_GROUP, rewritten).map_err(Into::into),
        MetadataRecordSet::QuotaReservation => insert_bytes(txn, QUOTA_RESERVATION, rewritten).map_err(Into::into),
        MetadataRecordSet::PolicyDecisionHistory => insert_bytes(txn, POLICY_DECISION, rewritten).map_err(Into::into),
        MetadataRecordSet::PolicyDecisionCurrent => {
            insert_text(txn, POLICY_DECISION_CURRENT, original, rewritten, migration, record_set)
        }
        MetadataRecordSet::PolicyDecisionCurrentById => {
            insert_bytes(txn, POLICY_DECISION_CURRENT_ID, rewritten).map_err(Into::into)
        }
        MetadataRecordSet::Analytics => insert_bytes(txn, ANALYTICS, rewritten).map_err(Into::into),
    }
}

fn rewrite_bytes(
    txn: &redb::WriteTransaction,
    definition: TableDefinition<'static, &'static str, &'static [u8]>,
    original: &MetadataRecord,
    rewritten: &MetadataRecord,
) -> Result<bool, MetaError> {
    let mut table = txn.open_table(definition)?;
    if original.key == rewritten.key {
        table.insert(rewritten.key.as_str(), rewritten.value.as_slice())?;
        return Ok(false);
    }
    let inserted = table.get(rewritten.key.as_str())?.is_none();
    if inserted {
        table.insert(rewritten.key.as_str(), rewritten.value.as_slice())?;
    }
    table.remove(original.key.as_str())?;
    Ok(inserted)
}

fn insert_bytes(
    txn: &redb::WriteTransaction,
    definition: TableDefinition<'static, &'static str, &'static [u8]>,
    rewritten: &MetadataRecord,
) -> Result<(), MetaError> {
    let mut table = txn.open_table(definition)?;
    if table.get(rewritten.key.as_str())?.is_none() {
        table.insert(rewritten.key.as_str(), rewritten.value.as_slice())?;
    }
    Ok(())
}

fn rewrite_text(
    txn: &redb::WriteTransaction,
    definition: TableDefinition<'static, &'static str, &'static str>,
    original: &MetadataRecord,
    rewritten: &MetadataRecord,
    migration: &'static str,
    record_set: MetadataRecordSet,
) -> Result<bool, MetadataMigrationError> {
    let value = rewritten_text(migration, record_set, original, rewritten)?;
    let mut table = txn.open_table(definition).map_err(MetaError::from)?;
    if original.key == rewritten.key {
        table.insert(rewritten.key.as_str(), value).map_err(MetaError::from)?;
        return Ok(false);
    }
    let inserted = table.get(rewritten.key.as_str()).map_err(MetaError::from)?.is_none();
    if inserted {
        table.insert(rewritten.key.as_str(), value).map_err(MetaError::from)?;
    }
    table.remove(original.key.as_str()).map_err(MetaError::from)?;
    Ok(inserted)
}

fn insert_text(
    txn: &redb::WriteTransaction,
    definition: TableDefinition<'static, &'static str, &'static str>,
    original: &MetadataRecord,
    rewritten: &MetadataRecord,
    migration: &'static str,
    record_set: MetadataRecordSet,
) -> Result<(), MetadataMigrationError> {
    let value = rewritten_text(migration, record_set, original, rewritten)?;
    let mut table = txn.open_table(definition).map_err(MetaError::from)?;
    if table.get(rewritten.key.as_str()).map_err(MetaError::from)?.is_none() {
        table.insert(rewritten.key.as_str(), value).map_err(MetaError::from)?;
    }
    Ok(())
}

fn rewritten_text<'a>(
    migration: &'static str,
    record_set: MetadataRecordSet,
    original: &MetadataRecord,
    rewritten: &'a MetadataRecord,
) -> Result<&'a str, MetadataMigrationError> {
    std::str::from_utf8(&rewritten.value).map_err(|_| MetadataMigrationError::InvalidText {
        migration,
        record_set,
        key: original.key.clone(),
    })
}

fn remove_legacy(txn: &redb::WriteTransaction, source: LegacyMetadataSource, key: &str) -> Result<(), MetaError> {
    match source.value_kind {
        MetadataValueKind::Bytes => {
            txn.open_table(TableDefinition::<&str, &[u8]>::new(source.table))?
                .remove(key)?;
        }
        MetadataValueKind::Text => {
            txn.open_table(TableDefinition::<&str, &str>::new(source.table))?
                .remove(key)?;
        }
    }
    Ok(())
}

fn legacy_table_empty(txn: &redb::WriteTransaction, source: LegacyMetadataSource) -> Result<bool, MetaError> {
    match source.value_kind {
        MetadataValueKind::Bytes => Ok(txn
            .open_table(TableDefinition::<&str, &[u8]>::new(source.table))?
            .iter()?
            .next()
            .transpose()?
            .is_none()),
        MetadataValueKind::Text => Ok(txn
            .open_table(TableDefinition::<&str, &str>::new(source.table))?
            .iter()?
            .next()
            .transpose()?
            .is_none()),
    }
}

fn delete_legacy_table(txn: &redb::WriteTransaction, source: LegacyMetadataSource) -> Result<(), MetaError> {
    match source.value_kind {
        MetadataValueKind::Bytes => {
            txn.delete_table(TableDefinition::<&str, &[u8]>::new(source.table))?;
        }
        MetadataValueKind::Text => {
            txn.delete_table(TableDefinition::<&str, &str>::new(source.table))?;
        }
    }
    Ok(())
}

const fn target_name(record_set: MetadataRecordSet) -> &'static str {
    match record_set {
        MetadataRecordSet::QuotaUsage => "quota_usage",
        MetadataRecordSet::QuotaResource => "quota_resource",
        MetadataRecordSet::QuotaGroup => "quota_group",
        MetadataRecordSet::QuotaReservation => "quota_reservation",
        MetadataRecordSet::PolicyDecisionHistory => "policy_decision",
        MetadataRecordSet::PolicyDecisionCurrent => "policy_decision_current",
        MetadataRecordSet::PolicyDecisionCurrentById => "policy_decision_current_id",
        MetadataRecordSet::Analytics => "analytics",
    }
}

const fn target_value_kind(record_set: MetadataRecordSet) -> MetadataValueKind {
    match record_set {
        MetadataRecordSet::PolicyDecisionCurrent => MetadataValueKind::Text,
        _ => MetadataValueKind::Bytes,
    }
}

#[cfg(test)]
#[path = "../../tests/unit/meta/migration_tests.rs"]
mod tests;
