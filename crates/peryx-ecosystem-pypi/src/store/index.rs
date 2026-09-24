use peryx_storage::meta::{DriverBatch, DriverTxn, MetaError, MetaScanError, MetaStore};

use super::attestations::replace_project_upstream_attestations_in_txn;
use super::record::{CachedIndex, CachedIndexPage, FreshnessOverlay, ProjectStatusRecord};
use super::{
    INDEX_PREFIX, file_key, file_source_value, freshness_key, index_key, project_status_key, publication_key,
    publication_prefix, publication_value, put_cached_project_row, remove_cached_project_row, retired_key,
};

/// Store everything a freshly fetched cached page produces in one transaction.
///
/// The cached page record, the observed project name, every file's source URL, and every PEP 658
/// sibling go in together. One transaction avoids a write per file, which made large projects
/// (numpy has thousands of files) take tens of seconds.
///
/// The commit is non-durable: page EOF waits on it so downloads always find their registrations, and
/// skipping the fsync keeps that wait at memory speed. The rows are re-fetchable cache data, so a
/// crash before the next durable commit only costs a refetch.
///
/// # Errors
/// Returns a store error if the write fails.
#[derive(Clone, Copy)]
pub struct CachedPageWrite<'a> {
    pub key: &'a str,
    pub record: &'a CachedIndex,
    pub index: &'a str,
    pub normalized: &'a str,
    pub display: &'a str,
    pub source: &'a str,
    pub upstream: Option<&'a str>,
    pub project_status: Option<&'a str>,
    pub project_status_reason: Option<&'a str>,
    pub files: &'a [PublishedFileWrite],
    pub attestations: &'a [(String, String, String)],
}

/// One file as a page published it: enough to register where its bytes live and what it said about
/// its PEP 658 sidecar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedFileWrite {
    pub sha256: String,
    pub filename: String,
    pub url: String,
    pub size: Option<u64>,
    /// `(sibling url, metadata sha256)` when the page advertised a sidecar for this file.
    pub metadata: Option<(String, String)>,
}

/// # Errors
/// Returns a store error if the transaction fails.
pub fn put_cached_page(meta: &MetaStore, write: CachedPageWrite<'_>) -> Result<(), MetaError> {
    let CachedPageWrite {
        key,
        record,
        index,
        normalized,
        display,
        source,
        upstream,
        project_status,
        project_status_reason,
        files,
        attestations,
    } = write;
    meta.commit_driver_cache_txn(|txn| {
        txn.put_local(&index_key(key), &record.encode())
            .and_then(|()| txn.remove(&freshness_key(key)).map(|_| ()))
            .and_then(|()| put_cached_project_row(txn, index, normalized, display))
            .and_then(|()| match (project_status, project_status_reason) {
                (None, None) => txn.remove(&project_status_key(index, normalized)).map(|_| ()),
                (status, reason) => serde_json::to_vec(&ProjectStatusRecord {
                    status: status.map(str::to_owned),
                    reason: reason.map(str::to_owned),
                })
                .map_err(MetaError::from)
                .and_then(|record| txn.put_local(&project_status_key(index, normalized), &record)),
            })
            .and_then(|()| {
                files.iter().try_for_each(|file| {
                    let value = file_source_value(&file.url, source, file.size, upstream);
                    txn.put_local(&file_key(index, normalized, &file.sha256), value.as_bytes())
                        .and_then(|()| {
                            let key = publication_key(index, normalized, &file.sha256, &file.filename);
                            let value = publication_value(file.metadata.as_ref(), source, upstream);
                            txn.put_local(&key, value.as_bytes())
                        })
                })
            })
            .and_then(|()| replace_project_upstream_attestations_in_txn(txn, index, normalized, upstream, attestations))
            // The page answers for the project again, so the retirement it carried is over.
            .and_then(|()| txn.remove_local(&retired_key(index, normalized)).map(|_| ()))
    })
}

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

/// Store a cached index record under `key` (for example `root-pypi/flask`), clearing any freshness
/// overlay a prior `304` left: a fresh body carries its own fetch time, which the overlay must not
/// shadow.
///
/// # Errors
/// Returns a store error if the write fails.
pub fn put_index(meta: &MetaStore, key: &str, record: &CachedIndex) -> Result<(), MetaError> {
    let mut batch = DriverBatch::new();
    batch.put(index_key(key), record.encode());
    batch.delete(freshness_key(key));
    meta.commit_driver_batch(&batch, true)
}

/// Retire an upstream project page and its provenance locators after an authoritative `404`.
///
/// # Errors
/// Returns a store error if the transaction fails.
pub fn retire_cached_project(meta: &MetaStore, key: &str, index: &str, project: &str) -> Result<(), MetaError> {
    meta.commit_driver_txn(|txn| {
        txn.remove(&index_key(key))
            .map(|_| ())
            .and_then(|()| txn.remove(&freshness_key(key)).map(|_| ()))
            .and_then(|()| remove_project_publications_in_txn(txn, index, project))
            .and_then(|()| replace_project_upstream_attestations_in_txn(txn, index, project, None, &[]))
            // The display row carries the project's name in the root list and its count in the index
            // summary, so it goes through the write path that keeps that count in step.
            .and_then(|()| remove_cached_project_row(txn, index, project).map(|_| ()))
            .and_then(|()| txn.remove_local(&project_status_key(index, project)).map(|_| ()))
            .and_then(|()| txn.put_local(&retired_key(index, project), &[]))
            .map(|()| ((), Vec::new()))
    })
}

/// Drop every publication record a project's page left behind, so a page peryx no longer holds
/// cannot keep answering with the sidecar it once advertised.
fn remove_project_publications_in_txn(txn: &mut DriverTxn<'_>, index: &str, normalized: &str) -> Result<(), MetaError> {
    txn.prefix(&publication_prefix(index, normalized))
        .and_then(|rows| rows.into_iter().try_for_each(|(key, _)| txn.remove(&key).map(|_| ())))
}

/// Advance a cached page's freshness after a `304 Not Modified`: write the small overlay row alone,
/// so the revalidation touches a header rather than rewriting the page body.
///
/// # Errors
/// Returns a store error if the write fails.
pub fn touch_index_freshness(
    meta: &MetaStore,
    key: &str,
    fetched_at_unix: i64,
    fresh_secs: Option<i64>,
) -> Result<(), MetaError> {
    let overlay = FreshnessOverlay {
        fetched_at_unix,
        fresh_secs,
    };
    let mut batch = DriverBatch::new();
    batch.put(freshness_key(key), overlay.encode());
    meta.commit_driver_batch(&batch, false)
}

/// # Errors
/// Returns a store error if the read fails or the stored bytes cannot be decoded.
pub fn get_index(meta: &MetaStore, key: &str) -> Result<Option<CachedIndex>, MetaError> {
    let Some(raw) = meta.get_driver_value(&index_key(key))? else {
        return Ok(None);
    };
    let mut record = CachedIndex::decode(&raw)?;
    if let Some(overlay) = read_overlay(meta, key)? {
        record.fetched_at_unix = overlay.fetched_at_unix;
        record.fresh_secs = overlay.fresh_secs;
    }
    Ok(Some(record))
}

fn read_overlay(meta: &MetaStore, key: &str) -> Result<Option<FreshnessOverlay>, MetaError> {
    Ok(meta
        .get_driver_value(&freshness_key(key))?
        .map(|raw| FreshnessOverlay::decode(&raw))
        .transpose()?)
}

/// Every cached page's key, fetch timestamp, and upstream freshness lifetime, for the
/// background refresher to find stale entries without loading the (potentially multi-megabyte)
/// bodies into a list.
///
/// # Errors
/// Returns a store error if the read fails or a stored record cannot be decoded.
///
pub fn list_index_pages(meta: &MetaStore) -> Result<Vec<(String, i64, Option<i64>)>, MetaError> {
    let mut pages = Vec::new();
    let mut error = None;
    meta.visit_driver_prefix(INDEX_PREFIX, |key, raw| {
        if error.is_some() {
            return;
        }
        let route = &key[INDEX_PREFIX.len()..];
        match read_overlay(meta, route).and_then(|overlay| {
            overlay.map_or_else(
                || CachedIndex::decode_freshness(raw).map_err(MetaError::from),
                |overlay| Ok((overlay.fetched_at_unix, overlay.fresh_secs)),
            )
        }) {
            Ok((fetched_at, fresh_secs)) => pages.push((route.to_owned(), fetched_at, fresh_secs)),
            Err(err) => error = Some(err),
        }
    })?;
    if let Some(err) = error {
        return Err(err);
    }
    Ok(pages)
}

/// Visit cached simple-index page summaries without collecting them.
///
/// # Errors
/// Returns a scan error if the store read fails, a record cannot be decoded, or the visitor
/// returns an error.
///
pub fn scan_index_pages<E>(
    meta: &MetaStore,
    mut visit: impl FnMut(CachedIndexPage) -> Result<(), E>,
) -> Result<(), MetaScanError<E>> {
    let mut error = None;
    meta.visit_driver_prefix(INDEX_PREFIX, |key, raw| {
        if error.is_some() {
            return;
        }
        error = (|| -> Result<(), MetaScanError<E>> {
            let mut summary = CachedIndex::summary(raw).map_err(MetaError::from)?;
            if let Some(overlay) = read_overlay(meta, &key[INDEX_PREFIX.len()..])? {
                summary.fetched_at_unix = overlay.fetched_at_unix;
                summary.fresh_secs = overlay.fresh_secs;
            }
            visit(CachedIndexPage {
                key: key[INDEX_PREFIX.len()..].to_owned(),
                summary,
            })
            .map_err(MetaScanError::Visit)
        })()
        .err();
    })?;
    if let Some(err) = error {
        return Err(err);
    }
    Ok(())
}

/// # Errors
/// Returns a scan error if the store read fails or the visitor returns an error.
///
pub fn scan_index_records<E>(
    meta: &MetaStore,
    mut visit: impl FnMut(&str, &[u8]) -> Result<(), E>,
) -> Result<(), MetaScanError<E>> {
    let mut error = None;
    meta.visit_driver_prefix(INDEX_PREFIX, |key, raw| {
        if error.is_none() {
            error = visit(&key[INDEX_PREFIX.len()..], raw).err();
        }
    })?;
    if let Some(err) = error {
        return Err(MetaScanError::Visit(err));
    }
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/unit/store/index/tests.rs"]
mod tests;
