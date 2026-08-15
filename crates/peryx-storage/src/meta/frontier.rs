use std::collections::BTreeMap;

use redb::ReadableTable as _;

use super::error::MetaError;
use super::{DERIVED_VIEW_FRONTIER, MetaStore, open_optional_table};

impl MetaStore {
    /// Advances the durable frontier monotonically. Reordered or replayed catch-up cannot move it
    /// backward or expose metadata beyond the applied view after restart.
    ///
    /// # Errors
    /// Returns a store error if the read or write fails.
    pub fn set_view_frontier(&self, view: &str, serial: u64) -> Result<u64, MetaError> {
        let txn = self.db.begin_write()?;
        let resulting = {
            let mut table = txn.open_table(DERIVED_VIEW_FRONTIER)?;
            let current = table.get(view)?.map_or(0, |value| value.value());
            let resulting = current.max(serial);
            if resulting != current {
                table.insert(view, resulting)?;
            }
            resulting
        };
        txn.commit()?;
        Ok(resulting)
    }

    /// Returns `None` when `view` has no recorded frontier.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn view_frontier(&self, view: &str) -> Result<Option<u64>, MetaError> {
        let txn = self.db.begin_read()?;
        let Some(table) = open_optional_table(&txn, DERIVED_VIEW_FRONTIER)? else {
            return Ok(None);
        };
        Ok(table.get(view)?.map(|value| value.value()))
    }

    /// Returns frontiers in view-name order.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn view_frontiers(&self) -> Result<BTreeMap<String, u64>, MetaError> {
        let txn = self.db.begin_read()?;
        let Some(table) = open_optional_table(&txn, DERIVED_VIEW_FRONTIER)? else {
            return Ok(BTreeMap::new());
        };
        let mut frontiers = BTreeMap::new();
        for entry in table.iter()? {
            let (view, serial) = entry?;
            frontiers.insert(view.value().to_owned(), serial.value());
        }
        Ok(frontiers)
    }
}

#[cfg(test)]
#[path = "../../tests/unit/meta/frontier/tests.rs"]
mod tests;
