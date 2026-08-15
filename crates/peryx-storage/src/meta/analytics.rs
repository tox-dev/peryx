use std::sync::Weak;

use super::error::MetaError;
use super::{
    ANALYTICS, ANALYTICS_APPLY_KEY, ANALYTICS_DAILY_KEY, ANALYTICS_KEY, ANALYTICS_PRODUCER_KEY, MetaDatabase, MetaStore,
};

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
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn load(&self) -> Result<Option<Vec<u8>>, MetaError> {
        self.read(ANALYTICS_KEY)
    }

    /// # Errors
    /// Returns a store error if the write fails.
    pub fn save(&self, snapshot: &[u8]) -> Result<(), MetaError> {
        self.write(ANALYTICS_KEY, snapshot)
    }

    /// Returns `None` before the first save or after the store drops. A separate key lets this format
    /// evolve independently from all-time per-artifact totals.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn load_daily(&self) -> Result<Option<Vec<u8>>, MetaError> {
        self.read(ANALYTICS_DAILY_KEY)
    }

    /// Does nothing after the store drops.
    ///
    /// # Errors
    /// Returns a store error if the write fails.
    pub fn save_daily(&self, snapshot: &[u8]) -> Result<(), MetaError> {
        self.write(ANALYTICS_DAILY_KEY, snapshot)
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
