use peryx_storage::meta::{DriverBatch, MetaError, MetaScanError, MetaStore};

use super::record::{CachedIndex, CachedIndexPage, ProjectStatusRecord};
use super::{
    INDEX_PREFIX, file_key, file_source_value, index_key, metadata_key, metadata_value, project_key, project_status_key,
};

/// Store everything a freshly fetched cached page produces in one transaction.
///
/// The cached page record, the observed project name, every file's source URL, and every PEP 658
/// sibling go in together. One commit means one fsync, where a write per file made large projects
/// (numpy has thousands of files) take tens of seconds.
///
/// The commit is non-durable: page EOF waits on it so downloads always find their registrations, and
/// skipping the fsync keeps that wait at memory speed. The rows are re-fetchable cache data, so a
/// crash before the next durable commit only costs a refetch.
///
/// # Errors
/// Returns a store error if the write fails.
#[allow(
    clippy::too_many_arguments,
    reason = "one transaction needs every namespace's rows together"
)]
pub fn put_cached_page(
    meta: &MetaStore,
    key: &str,
    record: &CachedIndex,
    index: &str,
    normalized: &str,
    display: &str,
    source: &str,
    project_status: Option<&str>,
    project_status_reason: Option<&str>,
    files: &[(String, String, Option<u64>)],
    metadata: &[(String, String, String)],
) -> Result<(), MetaError> {
    let mut batch = DriverBatch::new();
    batch.put(index_key(key), record.encode());
    batch.put(project_key(index, normalized), display.as_bytes().to_vec());
    match (project_status, project_status_reason) {
        (None, None) => batch.delete(project_status_key(index, normalized)),
        (status, reason) => {
            let record = serde_json::to_vec(&ProjectStatusRecord {
                status: status.map(str::to_owned),
                reason: reason.map(str::to_owned),
            })?;
            batch.put(project_status_key(index, normalized), record);
        }
    }
    for (sha256, url, size) in files {
        batch.put(file_key(sha256), file_source_value(url, source, *size).into_bytes());
    }
    for (wheel_sha256, url, metadata_sha256) in metadata {
        batch.put(
            metadata_key(wheel_sha256),
            metadata_value(url, metadata_sha256, source).into_bytes(),
        );
    }
    meta.commit_driver_batch(&batch, false)
}

/// Fetch one project's explicit status marker, if a cached upstream page provided one.
///
/// # Errors
/// Returns a store error if the read fails or the stored record cannot be decoded.
pub fn get_project_status(
    meta: &MetaStore,
    index: &str,
    normalized: &str,
) -> Result<Option<ProjectStatusRecord>, MetaError> {
    Ok(meta
        .get_driver_value(&project_status_key(index, normalized))?
        .map(|raw| serde_json::from_slice(&raw))
        .transpose()?)
}

/// Store a cached index record under `key` (for example `root/pypi/flask`).
///
/// # Errors
/// Returns a store error if the write fails.
pub fn put_index(meta: &MetaStore, key: &str, record: &CachedIndex) -> Result<(), MetaError> {
    meta.put_driver_value(&index_key(key), &record.encode())
}

/// Fetch a cached index record.
///
/// # Errors
/// Returns a store error if the read fails or the stored bytes cannot be decoded.
pub fn get_index(meta: &MetaStore, key: &str) -> Result<Option<CachedIndex>, MetaError> {
    Ok(meta
        .get_driver_value(&index_key(key))?
        .map(|raw| CachedIndex::decode(&raw))
        .transpose()?)
}

/// Every cached page's key, fetch timestamp, and upstream freshness lifetime, for the
/// background refresher to find stale entries without loading the (potentially multi-megabyte)
/// bodies into a list.
///
/// # Errors
/// Returns a store error if the read fails or a stored record cannot be decoded.
pub fn list_index_pages(meta: &MetaStore) -> Result<Vec<(String, i64, Option<i64>)>, MetaError> {
    let mut pages = Vec::new();
    for key in meta.driver_prefix_keys(INDEX_PREFIX)? {
        let (Some(logical), Some(raw)) = (key.strip_prefix(INDEX_PREFIX), meta.get_driver_value(&key)?) else {
            continue;
        };
        let (fetched_at, fresh_secs) = CachedIndex::decode_freshness(&raw)?;
        pages.push((logical.to_owned(), fetched_at, fresh_secs));
    }
    Ok(pages)
}

/// Visit cached simple-index page summaries without collecting them.
///
/// # Errors
/// Returns a scan error if the store read fails, a record cannot be decoded, or the visitor
/// returns an error.
pub fn scan_index_pages<E>(
    meta: &MetaStore,
    mut visit: impl FnMut(CachedIndexPage) -> Result<(), E>,
) -> Result<(), MetaScanError<E>> {
    for key in meta.driver_prefix_keys(INDEX_PREFIX)? {
        let (Some(logical), Some(raw)) = (key.strip_prefix(INDEX_PREFIX), meta.get_driver_value(&key)?) else {
            continue;
        };
        visit(CachedIndexPage {
            key: logical.to_owned(),
            summary: CachedIndex::summary(&raw).map_err(MetaError::from)?,
        })
        .map_err(MetaScanError::Visit)?;
    }
    Ok(())
}

/// Visit raw cached simple-index records, keyed by route.
///
/// # Errors
/// Returns a scan error if the store read fails or the visitor returns an error.
pub fn scan_index_records<E>(
    meta: &MetaStore,
    mut visit: impl FnMut(&str, &[u8]) -> Result<(), E>,
) -> Result<(), MetaScanError<E>> {
    for key in meta.driver_prefix_keys(INDEX_PREFIX)? {
        let (Some(logical), Some(raw)) = (key.strip_prefix(INDEX_PREFIX), meta.get_driver_value(&key)?) else {
            continue;
        };
        visit(logical, &raw).map_err(MetaScanError::Visit)?;
    }
    Ok(())
}
