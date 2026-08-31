//! A cross-datacenter copy pass plans a bounded slice of the placement index, so it records where the
//! scan stopped and the next pass resumes there instead of replanning the whole index every run.

use super::{BLOB_COPY_CURSOR, MetaError, MetaStore, open_optional_table};

impl MetaStore {
    /// Returns `None` when the datacenter's next pass starts at the first placement.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn blob_copy_cursor(&self, data_center: &str) -> Result<Option<String>, MetaError> {
        let txn = self.db.begin_read()?;
        let Some(table) = open_optional_table(&txn, BLOB_COPY_CURSOR)? else {
            return Ok(None);
        };
        Ok(table.get(data_center)?.map(|value| value.value().to_owned()))
    }

    /// `None` clears the cursor so the next pass restarts at the first placement.
    ///
    /// # Errors
    /// Returns a store error if the write fails.
    pub fn set_blob_copy_cursor(&self, data_center: &str, cursor: Option<&str>) -> Result<(), MetaError> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(BLOB_COPY_CURSOR)?;
            match cursor {
                Some(cursor) => {
                    table.insert(data_center, cursor)?;
                }
                None => {
                    table.remove(data_center)?;
                }
            }
        }
        txn.commit()?;
        Ok(())
    }
}
