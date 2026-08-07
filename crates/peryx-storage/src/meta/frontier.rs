use std::collections::BTreeMap;

use redb::ReadableTable as _;

use super::error::MetaError;
use super::{DERIVED_VIEW_FRONTIER, MetaStore};

impl MetaStore {
    /// Record that the derived view `view` has applied authoritative metadata through `serial`, and
    /// report the view's resulting frontier.
    ///
    /// The frontier never moves backward: a `serial` below the stored one leaves it untouched, so a
    /// reordered or replayed catch-up cannot un-apply proven work. The write is durable, so a restart
    /// reads the last frontier a rebuild actually reached rather than an optimistic one a crash never
    /// finished - a replica computing readability from it never exposes metadata past a view.
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

    /// The authoritative serial the derived view `view` has applied, or `None` when it recorded none.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn view_frontier(&self, view: &str) -> Result<Option<u64>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(DERIVED_VIEW_FRONTIER)?;
        Ok(table.get(view)?.map(|value| value.value()))
    }

    /// Every recorded derived-view frontier, keyed by view name in name order.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn view_frontiers(&self) -> Result<BTreeMap<String, u64>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(DERIVED_VIEW_FRONTIER)?;
        let mut frontiers = BTreeMap::new();
        for entry in table.iter()? {
            let (view, serial) = entry?;
            frontiers.insert(view.value().to_owned(), serial.value());
        }
        Ok(frontiers)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::MetaStore;

    #[test]
    fn test_view_frontier_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();

        assert_eq!(meta.view_frontier("search").unwrap(), None);
        assert_eq!(meta.view_frontiers().unwrap(), BTreeMap::new());
    }

    #[test]
    fn test_set_view_frontier_advances_and_reports_the_stored_serial() {
        let dir = tempfile::tempdir().unwrap();
        let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();

        assert_eq!(meta.set_view_frontier("search", 3).unwrap(), 3);
        assert_eq!(meta.view_frontier("search").unwrap(), Some(3));
        assert_eq!(meta.set_view_frontier("search", 7).unwrap(), 7);
        assert_eq!(meta.view_frontier("search").unwrap(), Some(7));
    }

    #[test]
    fn test_set_view_frontier_never_moves_backward() {
        let dir = tempfile::tempdir().unwrap();
        let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();

        assert_eq!(meta.set_view_frontier("search", 5).unwrap(), 5);
        assert_eq!(meta.set_view_frontier("search", 2).unwrap(), 5);
        assert_eq!(meta.view_frontier("search").unwrap(), Some(5));
    }

    #[test]
    fn test_view_frontiers_lists_every_view_in_name_order() {
        let dir = tempfile::tempdir().unwrap();
        let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
        meta.set_view_frontier("search", 4).unwrap();
        meta.set_view_frontier("cache", 2).unwrap();

        assert_eq!(
            meta.view_frontiers().unwrap(),
            BTreeMap::from([("cache".to_owned(), 2), ("search".to_owned(), 4)])
        );
    }

    #[test]
    fn test_view_frontier_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("peryx.redb");
        {
            let meta = MetaStore::open(&path).unwrap();
            meta.set_view_frontier("search", 9).unwrap();
        }
        let meta = MetaStore::open_existing(&path).unwrap();
        assert_eq!(meta.view_frontier("search").unwrap(), Some(9));
    }
}
