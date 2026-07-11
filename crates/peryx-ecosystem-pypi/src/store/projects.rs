use peryx_storage::meta::{DriverBatch, MetaError, MetaScanError, MetaStore};

use super::{PROJECTS_PREFIX, file_key, index_key, metadata_key, project_key, project_status_key};

/// Counts of metadata rows a project-cache purge plans or deletes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProjectCachePurgeCounts {
    pub index_pages: usize,
    pub project_records: usize,
    pub project_status_records: usize,
    pub file_url_records: usize,
    pub metadata_records: usize,
}

/// Record that `display` (a project's display name) has been observed on `index`, keyed by its
/// normalized name so re-observations do not duplicate.
///
/// # Errors
/// Returns a store error if the write fails.
pub fn put_project(meta: &MetaStore, index: &str, normalized: &str, display: &str) -> Result<(), MetaError> {
    meta.put_driver_value(&project_key(index, normalized), display.as_bytes())
}

/// Fetch a project's display name on one index.
///
/// # Errors
/// Returns a store error if the read fails.
pub fn get_project(meta: &MetaStore, index: &str, normalized: &str) -> Result<Option<String>, MetaError> {
    Ok(meta
        .get_driver_value(&project_key(index, normalized))?
        .and_then(|raw| String::from_utf8(raw).ok()))
}

/// Visit raw project-display records, keyed by `{index}/{normalized}`.
///
/// # Errors
/// Returns a scan error if the store read fails or the visitor returns an error.
pub fn scan_project_records<E>(
    meta: &MetaStore,
    mut visit: impl FnMut(&str, &str) -> Result<(), E>,
) -> Result<(), MetaScanError<E>> {
    for key in meta.driver_prefix_keys(PROJECTS_PREFIX)? {
        let (Some(logical), Some(raw)) = (key.strip_prefix(PROJECTS_PREFIX), meta.get_driver_value(&key)?) else {
            continue;
        };
        let Ok(value) = String::from_utf8(raw) else {
            continue;
        };
        visit(logical, &value).map_err(MetaScanError::Visit)?;
    }
    Ok(())
}

/// List the display names of projects observed on `index`, sorted.
///
/// # Errors
/// Returns a store error if the read fails.
pub fn list_projects(meta: &MetaStore, index: &str) -> Result<Vec<String>, MetaError> {
    let prefix = format!("{PROJECTS_PREFIX}{index}/");
    let mut names = Vec::new();
    for key in meta.driver_prefix_keys(&prefix)? {
        if let Some(display) = meta.get_driver_value(&key)?.and_then(|raw| String::from_utf8(raw).ok()) {
            names.push(display);
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
    meta: &MetaStore,
    index: &str,
    normalized: &str,
    file_digests: &[String],
    metadata_digests: &[String],
) -> Result<ProjectCachePurgeCounts, MetaError> {
    let key = format!("{index}/{normalized}");
    let mut file_url_records = 0;
    for digest in file_digests {
        file_url_records += usize::from(meta.get_driver_value(&file_key(digest))?.is_some());
    }
    let mut metadata_records = 0;
    for digest in metadata_digests {
        metadata_records += usize::from(meta.get_driver_value(&metadata_key(digest))?.is_some());
    }
    Ok(ProjectCachePurgeCounts {
        index_pages: usize::from(meta.get_driver_value(&index_key(&key))?.is_some()),
        project_records: usize::from(meta.get_driver_value(&project_key(index, normalized))?.is_some()),
        project_status_records: usize::from(meta.get_driver_value(&project_status_key(index, normalized))?.is_some()),
        file_url_records,
        metadata_records,
    })
}

/// Delete cached metadata rows for one project, in one transaction, reporting what was removed.
///
/// # Errors
/// Returns a store error if the write fails.
pub fn delete_project_cache(
    meta: &MetaStore,
    index: &str,
    normalized: &str,
    file_digests: &[String],
    metadata_digests: &[String],
) -> Result<ProjectCachePurgeCounts, MetaError> {
    let counts = count_project_cache_purge(meta, index, normalized, file_digests, metadata_digests)?;
    let key = format!("{index}/{normalized}");
    let mut batch = DriverBatch::new();
    batch.delete(index_key(&key));
    batch.delete(project_key(index, normalized));
    batch.delete(project_status_key(index, normalized));
    for digest in file_digests {
        batch.delete(file_key(digest));
    }
    for digest in metadata_digests {
        batch.delete(metadata_key(digest));
    }
    meta.commit_driver_batch(&batch, true)?;
    Ok(counts)
}
