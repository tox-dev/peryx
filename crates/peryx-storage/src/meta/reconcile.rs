use std::num::NonZeroUsize;
use std::ops::Bound::{Excluded, Unbounded};

use peryx_ha::{NewReconcileEntry, ReconcileEnqueue, ReconcileEntry, ReconcilePage, ReconcileStore};
use redb::{ReadableTable as _, ReadableTableMetadata as _};

use super::{MetaError, MetaStore, RECONCILE_BACKLOG, open_optional_table};

impl MetaStore {
    /// Inserts a backlog record only when its stable key is absent.
    ///
    /// # Errors
    /// Returns a store error when the transaction cannot be read, encoded, or committed.
    pub fn enqueue_reconcile(&self, entry: &NewReconcileEntry<'_>, now: i64) -> Result<ReconcileEnqueue, MetaError> {
        let key = entry.key();
        let txn = self.db.begin_write()?;
        let outcome = {
            let mut table = txn.open_table(RECONCILE_BACKLOG)?;
            if table.get(key.as_str())?.is_some() {
                ReconcileEnqueue::AlreadyPresent
            } else {
                table.insert(key.as_str(), serde_json::to_vec(&entry.record(now))?.as_slice())?;
                ReconcileEnqueue::Enqueued
            }
        };
        txn.commit()?;
        Ok(outcome)
    }

    /// # Errors
    /// Returns a store error when a row cannot be read or decoded.
    pub fn pending_reconcile(&self, limit: usize) -> Result<Vec<(String, ReconcileEntry)>, MetaError> {
        self.reconcile_rows(limit, ReconcileEntry::is_pending)
    }

    /// # Errors
    /// Returns a store error when a row cannot be read or decoded.
    pub fn settled_reconcile(&self, limit: usize) -> Result<Vec<(String, ReconcileEntry)>, MetaError> {
        self.reconcile_rows(limit, |entry| !entry.is_pending())
    }

    /// # Errors
    /// Returns a store error when a row cannot be read or decoded.
    pub fn scan_reconcile(&self, cursor: Option<&str>, limit: NonZeroUsize) -> Result<ReconcilePage, MetaError> {
        let txn = self.db.begin_read()?;
        let Some(table) = open_optional_table(&txn, RECONCILE_BACKLOG)? else {
            return Ok(ReconcilePage::default());
        };
        let entries = match cursor {
            Some(after) => table.range::<&str>((Excluded(after), Unbounded))?,
            None => table.iter()?,
        };
        let mut page = ReconcilePage::default();
        for entry in entries {
            let (key, value) = entry?;
            let key = key.value().to_owned();
            page.records.push((key.clone(), serde_json::from_slice(value.value())?));
            if page.records.len() == limit.get() {
                page.next_cursor = Some(key);
                break;
            }
        }
        Ok(page)
    }

    fn reconcile_rows(
        &self,
        limit: usize,
        include: impl Fn(&ReconcileEntry) -> bool,
    ) -> Result<Vec<(String, ReconcileEntry)>, MetaError> {
        let txn = self.db.begin_read()?;
        let Some(table) = open_optional_table(&txn, RECONCILE_BACKLOG)? else {
            return Ok(Vec::new());
        };
        let mut rows = Vec::new();
        for entry in table.iter()? {
            if rows.len() >= limit {
                break;
            }
            let (key, value) = entry?;
            let record: ReconcileEntry = serde_json::from_slice(value.value())?;
            if include(&record) {
                rows.push((key.value().to_owned(), record));
            }
        }
        Ok(rows)
    }

    /// Sets an outcome only when the row remains pending.
    ///
    /// # Errors
    /// Returns a store error when the transaction cannot be read, encoded, or committed.
    pub fn settle_reconcile(&self, key: &str, outcome: &str, now: i64) -> Result<bool, MetaError> {
        let txn = self.db.begin_write()?;
        let settled = {
            let mut table = txn.open_table(RECONCILE_BACKLOG)?;
            let existing = table
                .get(key)?
                .map(|value| serde_json::from_slice::<ReconcileEntry>(value.value()))
                .transpose()?;
            match existing {
                Some(mut record) if record.is_pending() => {
                    record.outcome = Some(outcome.to_owned());
                    record.updated_at_unix = now;
                    table.insert(key, serde_json::to_vec(&record)?.as_slice())?;
                    true
                }
                _ => false,
            }
        };
        txn.commit()?;
        Ok(settled)
    }

    /// Removes a row only when its complete record remains unchanged.
    ///
    /// # Errors
    /// Returns a store error when the transaction cannot be read or committed.
    pub fn compare_and_remove_reconcile(&self, key: &str, expected: &ReconcileEntry) -> Result<bool, MetaError> {
        let txn = self.db.begin_write()?;
        let removed = {
            let mut table = txn.open_table(RECONCILE_BACKLOG)?;
            let current = table
                .get(key)?
                .map(|value| serde_json::from_slice::<ReconcileEntry>(value.value()))
                .transpose()?;
            if current.as_ref() == Some(expected) {
                table.remove(key)?;
                true
            } else {
                false
            }
        };
        txn.commit()?;
        Ok(removed)
    }

    /// # Errors
    /// Returns a store error when the row cannot be read or decoded.
    pub fn reconcile_entry(&self, key: &str) -> Result<Option<ReconcileEntry>, MetaError> {
        let txn = self.db.begin_read()?;
        let Some(table) = open_optional_table(&txn, RECONCILE_BACKLOG)? else {
            return Ok(None);
        };
        Ok(table
            .get(key)?
            .map(|value| serde_json::from_slice(value.value()))
            .transpose()?)
    }

    /// # Errors
    /// Returns a store error when the table cannot be read.
    pub fn count_reconcile(&self) -> Result<u64, MetaError> {
        let txn = self.db.begin_read()?;
        let Some(table) = open_optional_table(&txn, RECONCILE_BACKLOG)? else {
            return Ok(0);
        };
        Ok(table.len()?)
    }
}

impl ReconcileStore for MetaStore {
    type Error = MetaError;

    fn enqueue_reconcile(&self, entry: &NewReconcileEntry<'_>, now: i64) -> Result<ReconcileEnqueue, Self::Error> {
        Self::enqueue_reconcile(self, entry, now)
    }

    fn pending_reconcile(&self, limit: usize) -> Result<Vec<(String, ReconcileEntry)>, Self::Error> {
        Self::pending_reconcile(self, limit)
    }

    fn settled_reconcile(&self, limit: usize) -> Result<Vec<(String, ReconcileEntry)>, Self::Error> {
        Self::settled_reconcile(self, limit)
    }

    fn scan_reconcile(&self, cursor: Option<&str>, limit: NonZeroUsize) -> Result<ReconcilePage, Self::Error> {
        Self::scan_reconcile(self, cursor, limit)
    }

    fn settle_reconcile(&self, key: &str, outcome: &str, now: i64) -> Result<bool, Self::Error> {
        Self::settle_reconcile(self, key, outcome, now)
    }

    fn compare_and_remove_reconcile(&self, key: &str, expected: &ReconcileEntry) -> Result<bool, Self::Error> {
        Self::compare_and_remove_reconcile(self, key, expected)
    }

    fn reconcile_entry(&self, key: &str) -> Result<Option<ReconcileEntry>, Self::Error> {
        Self::reconcile_entry(self, key)
    }
}

#[cfg(test)]
#[path = "../../tests/unit/meta/reconcile/tests.rs"]
mod tests;
