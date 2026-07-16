use std::sync::Weak;

use redb::{Database, ReadableDatabase as _};

use super::error::MetaError;
use super::{ANALYTICS, ANALYTICS_KEY, MetaStore};

/// A shared, `Clone`-cheap handle onto the metadata store's analytics table.
///
/// The metrics aggregator holds one to persist and restore download aggregates off the request path.
/// It borrows the store's database weakly, so the aggregator thread can outlive the store without
/// pinning the redb file lock: once the [`MetaStore`] drops, the handle's reads and writes turn into
/// no-ops instead of keeping the database open.
#[derive(Debug, Clone)]
pub struct AnalyticsHandle {
    db: Weak<Database>,
}

impl MetaStore {
    /// A handle the metrics aggregator uses to persist and restore download aggregates.
    #[must_use]
    pub fn analytics(&self) -> AnalyticsHandle {
        AnalyticsHandle {
            db: std::sync::Arc::downgrade(&self.db),
        }
    }
}

impl AnalyticsHandle {
    /// Read the persisted download-aggregate snapshot, or `None` before the first save or after the
    /// store has dropped.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn load(&self) -> Result<Option<Vec<u8>>, MetaError> {
        let Some(db) = self.db.upgrade() else { return Ok(None) };
        let txn = db.begin_read()?;
        let table = txn.open_table(ANALYTICS)?;
        Ok(table.get(ANALYTICS_KEY)?.map(|value| value.value().to_vec()))
    }

    /// Overwrite the persisted download-aggregate snapshot with `snapshot`, or do nothing once the
    /// store has dropped.
    ///
    /// # Errors
    /// Returns a store error if the write fails.
    pub fn save(&self, snapshot: &[u8]) -> Result<(), MetaError> {
        let Some(db) = self.db.upgrade() else { return Ok(()) };
        let txn = db.begin_write()?;
        {
            let mut table = txn.open_table(ANALYTICS)?;
            table.insert(ANALYTICS_KEY, snapshot)?;
        }
        txn.commit()?;
        Ok(())
    }
}
