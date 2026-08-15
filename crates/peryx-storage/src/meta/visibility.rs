//! Stores the replication layer's opaque visibility state so followers retain tombstones across restarts
//! and log compaction without coupling storage to its encoding.

use peryx_ha::VisibilitySnapshotStore;

use super::{MetaError, MetaStore, VISIBILITY_SNAPSHOT, VISIBILITY_SNAPSHOT_KEY, open_optional_table};

impl MetaStore {
    /// Returns `None` before the first save.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn visibility_snapshot(&self) -> Result<Option<Vec<u8>>, MetaError> {
        let txn = self.db.begin_read()?;
        let Some(table) = open_optional_table(&txn, VISIBILITY_SNAPSHOT)? else {
            return Ok(None);
        };
        Ok(table.get(VISIBILITY_SNAPSHOT_KEY)?.map(|value| value.value().to_vec()))
    }

    /// # Errors
    /// Returns a store error if the write fails.
    pub fn save_visibility_snapshot(&self, snapshot: &[u8]) -> Result<(), MetaError> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(VISIBILITY_SNAPSHOT)?;
            table.insert(VISIBILITY_SNAPSHOT_KEY, snapshot)?;
        }
        txn.commit()?;
        Ok(())
    }
}

impl VisibilitySnapshotStore for MetaStore {
    type Error = MetaError;

    fn load_snapshot(&self) -> Result<Option<Vec<u8>>, Self::Error> {
        self.visibility_snapshot()
    }

    fn save_snapshot(&self, bytes: &[u8]) -> Result<(), Self::Error> {
        self.save_visibility_snapshot(bytes)
    }
}
