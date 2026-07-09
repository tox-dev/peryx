use redb::{ReadableDatabase as _, ReadableTable as _};

use super::error::{MetaError, MetaScanError};
use super::{FILE, INDEX, METADATA, MetaStore, PROJECT_STATUS, PROJECTS};

/// Counts of metadata rows a project-cache purge plans or deletes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProjectCachePurgeCounts {
    pub index_pages: usize,
    pub project_records: usize,
    pub project_status_records: usize,
    pub file_url_records: usize,
    pub metadata_records: usize,
}

impl MetaStore {
    /// Record that `display` (a project's display name) has been observed on `index`, keyed by its
    /// normalized name so re-observations do not duplicate.
    ///
    /// # Errors
    /// Returns a store error if the write or commit fails.
    pub fn put_project(&self, index: &str, normalized: &str, display: &str) -> Result<(), MetaError> {
        let key = format!("{index}/{normalized}");
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(PROJECTS)?;
            table.insert(key.as_str(), display)?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Fetch a project's display name on one index.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn get_project(&self, index: &str, normalized: &str) -> Result<Option<String>, MetaError> {
        let key = format!("{index}/{normalized}");
        let txn = self.db.begin_read()?;
        let table = txn.open_table(PROJECTS)?;
        Ok(table.get(key.as_str())?.map(|value| value.value().to_owned()))
    }

    /// Visit raw project-display records.
    ///
    /// # Errors
    /// Returns a scan error if the store read fails or the visitor returns an error.
    pub fn scan_project_records<E>(
        &self,
        mut visit: impl FnMut(&str, &str) -> Result<(), E>,
    ) -> Result<(), MetaScanError<E>> {
        let txn = self.db.begin_read().map_err(MetaError::from)?;
        let table = txn.open_table(PROJECTS).map_err(MetaError::from)?;
        for entry in table.iter().map_err(MetaError::from)? {
            let (key, value) = entry.map_err(MetaError::from)?;
            visit(key.value(), value.value()).map_err(MetaScanError::Visit)?;
        }
        Ok(())
    }

    /// List the display names of projects observed on `index`, sorted.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn list_projects(&self, index: &str) -> Result<Vec<String>, MetaError> {
        let prefix = format!("{index}/");
        let txn = self.db.begin_read()?;
        let table = txn.open_table(PROJECTS)?;
        let mut names = Vec::new();
        for entry in table.iter()? {
            let (key, value) = entry?;
            if key.value().starts_with(&prefix) {
                names.push(value.value().to_owned());
            }
        }
        names.sort();
        Ok(names)
    }

    /// Count the rows a project-cache purge would remove.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn count_project_cache_purge(
        &self,
        index: &str,
        normalized: &str,
        file_digests: &[String],
        metadata_digests: &[String],
    ) -> Result<ProjectCachePurgeCounts, MetaError> {
        let key = format!("{index}/{normalized}");
        let txn = self.db.begin_read()?;
        let index_pages = usize::from(txn.open_table(INDEX)?.get(key.as_str())?.is_some());
        let project_records = usize::from(txn.open_table(PROJECTS)?.get(key.as_str())?.is_some());
        let project_status_records = usize::from(txn.open_table(PROJECT_STATUS)?.get(key.as_str())?.is_some());
        let file_table = txn.open_table(FILE)?;
        let mut file_url_records = 0;
        for digest in file_digests {
            file_url_records += usize::from(file_table.get(digest.as_str())?.is_some());
        }
        let metadata_table = txn.open_table(METADATA)?;
        let mut metadata_records = 0;
        for digest in metadata_digests {
            metadata_records += usize::from(metadata_table.get(digest.as_str())?.is_some());
        }
        Ok(ProjectCachePurgeCounts {
            index_pages,
            project_records,
            project_status_records,
            file_url_records,
            metadata_records,
        })
    }

    /// Delete cached metadata rows for one project.
    ///
    /// # Errors
    /// Returns a store error if the write or commit fails.
    pub fn delete_project_cache(
        &self,
        index: &str,
        normalized: &str,
        file_digests: &[String],
        metadata_digests: &[String],
    ) -> Result<ProjectCachePurgeCounts, MetaError> {
        let key = format!("{index}/{normalized}");
        let txn = self.db.begin_write()?;
        let counts = {
            let index_pages = {
                let mut table = txn.open_table(INDEX)?;
                usize::from(table.remove(key.as_str())?.is_some())
            };
            let project_records = {
                let mut table = txn.open_table(PROJECTS)?;
                usize::from(table.remove(key.as_str())?.is_some())
            };
            let project_status_records = {
                let mut table = txn.open_table(PROJECT_STATUS)?;
                usize::from(table.remove(key.as_str())?.is_some())
            };
            let file_url_records = {
                let mut table = txn.open_table(FILE)?;
                let mut removed = 0;
                for digest in file_digests {
                    removed += usize::from(table.remove(digest.as_str())?.is_some());
                }
                removed
            };
            let metadata_records = {
                let mut table = txn.open_table(METADATA)?;
                let mut removed = 0;
                for digest in metadata_digests {
                    removed += usize::from(table.remove(digest.as_str())?.is_some());
                }
                removed
            };
            ProjectCachePurgeCounts {
                index_pages,
                project_records,
                project_status_records,
                file_url_records,
                metadata_records,
            }
        };
        txn.commit()?;
        Ok(counts)
    }
}
