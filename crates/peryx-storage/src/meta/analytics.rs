use std::sync::Weak;

use super::error::MetaError;
use super::{
    ANALYTICS, ANALYTICS_APPLY_KEY, ANALYTICS_DAILY_KEY, ANALYTICS_KEY, ANALYTICS_PRODUCER_KEY, MetaDatabase, MetaStore,
};

/// Serialized lifetime and daily metrics from one event frontier.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AnalyticsCheckpoint {
    /// All-time per-artifact aggregates.
    pub lifetime: Option<Vec<u8>>,
    /// Retained daily aggregates.
    pub daily: Option<Vec<u8>>,
}

/// Does not keep the database open.
#[derive(Debug, Clone)]
pub struct AnalyticsHandle {
    db: Weak<MetaDatabase>,
}

impl MetaStore {
    #[must_use]
    pub fn analytics(&self) -> AnalyticsHandle {
        AnalyticsHandle {
            db: std::sync::Arc::downgrade(&self.db),
        }
    }
}

impl AnalyticsHandle {
    /// Loads both snapshots from one storage snapshot. Returns two absent snapshots before the first
    /// checkpoint or after the store drops.
    ///
    /// # Errors
    /// Returns a store error if either snapshot cannot be read.
    pub fn load_checkpoint(&self) -> Result<AnalyticsCheckpoint, MetaError> {
        let Some(db) = self.db.upgrade() else {
            return Ok(AnalyticsCheckpoint::default());
        };
        let txn = db.begin_read()?;
        let table = txn.open_table(ANALYTICS)?;
        Ok(AnalyticsCheckpoint {
            lifetime: table.get(ANALYTICS_KEY)?.map(|value| value.value().to_vec()),
            daily: table.get(ANALYTICS_DAILY_KEY)?.map(|value| value.value().to_vec()),
        })
    }

    /// Saves the lifetime and daily snapshots in one transaction. Does nothing after the store drops.
    ///
    /// # Errors
    /// Returns a store error if either snapshot cannot be written or the transaction cannot commit.
    pub fn save_checkpoint(&self, lifetime: &[u8], daily: &[u8]) -> Result<(), MetaError> {
        let Some(db) = self.db.upgrade() else {
            return Ok(());
        };
        let txn = db.begin_write()?;
        {
            let mut table = txn.open_table(ANALYTICS)?;
            table.insert(ANALYTICS_KEY, lifetime)?;
            table.insert(ANALYTICS_DAILY_KEY, daily)?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Returns `None` before the first save or after the store drops.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn load_apply(&self) -> Result<Option<Vec<u8>>, MetaError> {
        self.read(ANALYTICS_APPLY_KEY)
    }

    /// Does nothing after the store drops.
    ///
    /// # Errors
    /// Returns a store error if the write fails.
    pub fn save_apply(&self, snapshot: &[u8]) -> Result<(), MetaError> {
        self.write(ANALYTICS_APPLY_KEY, snapshot)
    }

    /// Returns `None` before the first save or after the store drops.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn load_producer(&self) -> Result<Option<Vec<u8>>, MetaError> {
        self.read(ANALYTICS_PRODUCER_KEY)
    }

    /// Does nothing after the store drops.
    ///
    /// # Errors
    /// Returns a store error if the write fails.
    pub fn save_producer(&self, snapshot: &[u8]) -> Result<(), MetaError> {
        self.write(ANALYTICS_PRODUCER_KEY, snapshot)
    }

    fn read(&self, key: &str) -> Result<Option<Vec<u8>>, MetaError> {
        let Some(db) = self.db.upgrade() else {
            return Ok(None);
        };
        let txn = db.begin_read()?;
        let table = txn.open_table(ANALYTICS)?;
        Ok(table.get(key)?.map(|value| value.value().to_vec()))
    }

    fn write(&self, key: &str, snapshot: &[u8]) -> Result<(), MetaError> {
        let Some(db) = self.db.upgrade() else {
            return Ok(());
        };
        let txn = db.begin_write()?;
        {
            let mut table = txn.open_table(ANALYTICS)?;
            table.insert(key, snapshot)?;
        }
        txn.commit()?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "../../tests/unit/meta/analytics_fault_tests.rs"]
mod fault_tests;
