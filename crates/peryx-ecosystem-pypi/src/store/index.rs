use peryx_storage::meta::{DriverBatch, MetaError, MetaScanError, MetaStore};

use super::record::{CachedIndex, CachedIndexPage, FreshnessOverlay, ProjectStatusRecord};
use super::{
    INDEX_PREFIX, file_key, file_source_value, freshness_key, index_key, metadata_key, metadata_value, project_key,
    project_status_key,
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
    batch.delete(freshness_key(key));
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

/// Store a cached index record under `key` (for example `root/pypi/flask`), clearing any freshness
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

/// Fetch a cached index record, with any freshness a later `304` advanced folded in over the body
/// row's own timestamp.
///
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

/// The freshness overlay a `304` left for `key`, if any.
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
/// # Panics
/// Never in practice: a key the prefix scan just returned still has its value.
pub fn list_index_pages(meta: &MetaStore) -> Result<Vec<(String, i64, Option<i64>)>, MetaError> {
    let mut pages = Vec::new();
    for key in meta.driver_prefix_keys(INDEX_PREFIX)? {
        let route = &key[INDEX_PREFIX.len()..];
        let (fetched_at, fresh_secs) = if let Some(overlay) = read_overlay(meta, route)? {
            (overlay.fetched_at_unix, overlay.fresh_secs)
        } else {
            let raw = meta
                .get_driver_value(&key)?
                .expect("a key from the prefix scan still has its value");
            CachedIndex::decode_freshness(&raw)?
        };
        pages.push((route.to_owned(), fetched_at, fresh_secs));
    }
    Ok(pages)
}

/// Visit cached simple-index page summaries without collecting them.
///
/// # Errors
/// Returns a scan error if the store read fails, a record cannot be decoded, or the visitor
/// returns an error.
///
/// # Panics
/// Never in practice: a key the prefix scan just returned still has its value.
pub fn scan_index_pages<E>(
    meta: &MetaStore,
    mut visit: impl FnMut(CachedIndexPage) -> Result<(), E>,
) -> Result<(), MetaScanError<E>> {
    for key in meta.driver_prefix_keys(INDEX_PREFIX)? {
        let raw = meta
            .get_driver_value(&key)?
            .expect("a key from the prefix scan still has its value");
        let mut summary = CachedIndex::summary(&raw).map_err(MetaError::from)?;
        if let Some(overlay) = read_overlay(meta, &key[INDEX_PREFIX.len()..])? {
            summary.fetched_at_unix = overlay.fetched_at_unix;
            summary.fresh_secs = overlay.fresh_secs;
        }
        visit(CachedIndexPage {
            key: key[INDEX_PREFIX.len()..].to_owned(),
            summary,
        })
        .map_err(MetaScanError::Visit)?;
    }
    Ok(())
}

/// Visit raw cached simple-index records, keyed by route.
///
/// # Errors
/// Returns a scan error if the store read fails or the visitor returns an error.
///
/// # Panics
/// Never in practice: a key the prefix scan just returned still has its value.
pub fn scan_index_records<E>(
    meta: &MetaStore,
    mut visit: impl FnMut(&str, &[u8]) -> Result<(), E>,
) -> Result<(), MetaScanError<E>> {
    for key in meta.driver_prefix_keys(INDEX_PREFIX)? {
        let raw = meta
            .get_driver_value(&key)?
            .expect("a key from the prefix scan still has its value");
        visit(&key[INDEX_PREFIX.len()..], &raw).map_err(MetaScanError::Visit)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use super::{CachedIndex, MetaStore, index_key};
    use crate::store::PypiStore as _;

    fn store() -> (tempfile::TempDir, MetaStore) {
        let dir = tempfile::tempdir().unwrap();
        let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
        (dir, meta)
    }

    fn record() -> CachedIndex {
        CachedIndex {
            etag: Some("\"abc\"".to_owned()),
            last_serial: Some(42),
            fetched_at_unix: 1_700_000_000,
            content_type: None,
            fresh_secs: None,
            body: b"<html></html>".to_vec(),
        }
    }

    #[test]
    fn test_put_and_get_index_roundtrip() {
        let (_dir, meta) = store();
        assert_eq!(meta.get_index("root/pypi/flask").unwrap(), None);
        meta.put_index("root/pypi/flask", &record()).unwrap();
        assert_eq!(meta.get_index("root/pypi/flask").unwrap(), Some(record()));
    }

    #[test]
    fn test_put_index_overwrites() {
        let (_dir, meta) = store();
        meta.put_index("k", &record()).unwrap();
        let mut updated = record();
        updated.last_serial = Some(99);
        meta.put_index("k", &updated).unwrap();
        assert_eq!(meta.get_index("k").unwrap().unwrap().last_serial, Some(99));
    }

    #[test]
    fn test_touch_index_freshness_advances_without_rewriting_the_body_row() {
        let (_dir, meta) = store();
        meta.put_index("pypi/flask", &record()).unwrap();
        let body_row = meta.get_driver_value(&index_key("pypi/flask")).unwrap().unwrap();

        meta.touch_index_freshness("pypi/flask", 1_800_000_000, Some(900))
            .unwrap();

        assert_eq!(
            meta.get_driver_value(&index_key("pypi/flask")).unwrap().unwrap(),
            body_row,
            "a 304 rewrites the freshness overlay, not the page body row"
        );
        let refreshed = meta.get_index("pypi/flask").unwrap().unwrap();
        assert_eq!(refreshed.fetched_at_unix, 1_800_000_000);
        assert_eq!(refreshed.fresh_secs, Some(900));
        assert_eq!(refreshed.body, record().body, "the served body is unchanged");
        assert_eq!(refreshed.etag, record().etag);
    }

    #[test]
    fn test_put_index_clears_a_stale_freshness_overlay() {
        let (_dir, meta) = store();
        meta.put_index("k", &record()).unwrap();
        meta.touch_index_freshness("k", 9_999, Some(1)).unwrap();

        let mut replaced = record();
        replaced.fetched_at_unix = 2_000_000_000;
        replaced.body = b"<html>new</html>".to_vec();
        meta.put_index("k", &replaced).unwrap();

        assert_eq!(
            meta.get_index("k").unwrap().unwrap(),
            replaced,
            "a 200 replaces the body and its freshness; the overlay must not shadow it"
        );
    }

    #[test]
    fn test_list_index_pages_reflects_a_freshness_overlay() {
        let (_dir, meta) = store();
        meta.put_index("pypi/flask", &record()).unwrap();
        meta.touch_index_freshness("pypi/flask", 1_900_000_000, Some(120))
            .unwrap();
        assert_eq!(
            meta.list_index_pages().unwrap(),
            vec![("pypi/flask".to_owned(), 1_900_000_000, Some(120))]
        );
    }

    #[test]
    fn test_scan_index_pages_reflects_a_freshness_overlay() {
        let (_dir, meta) = store();
        meta.put_index("pypi/flask", &record()).unwrap();
        meta.touch_index_freshness("pypi/flask", 1_900_000_000, Some(120))
            .unwrap();
        let mut pages = Vec::new();
        meta.scan_index_pages(|page| {
            pages.push((
                page.key,
                page.summary.fetched_at_unix,
                page.summary.fresh_secs,
                page.summary.body_bytes,
            ));
            Ok::<(), std::io::Error>(())
        })
        .unwrap();
        assert_eq!(pages, vec![("pypi/flask".to_owned(), 1_900_000_000, Some(120), 13)]);
    }

    #[test]
    fn test_put_cached_page_records_file_url_size_and_status() {
        let (_dir, meta) = store();
        meta.put_cached_page(
            "pypi/pkg",
            &record(),
            "pypi",
            "pkg",
            "Pkg",
            "pypi",
            Some("archived"),
            Some("read only"),
            &[(
                "feedface".to_owned(),
                "https://files.example/pkg-1.0.whl".to_owned(),
                Some(42),
            )],
            &[],
        )
        .unwrap();

        assert_eq!(
            meta.get_file_url("feedface").unwrap().unwrap().size,
            Some(42),
            "the file's size line round-trips"
        );
        assert_eq!(
            meta.get_project_status("pypi", "pkg")
                .unwrap()
                .unwrap()
                .status
                .as_deref(),
            Some("archived")
        );
    }

    #[test]
    fn test_put_cached_page_clears_status_when_none() {
        let (_dir, meta) = store();
        meta.put_cached_page(
            "pypi/pkg",
            &record(),
            "pypi",
            "pkg",
            "Pkg",
            "pypi",
            None,
            None,
            &[],
            &[],
        )
        .unwrap();
        assert!(meta.get_project_status("pypi", "pkg").unwrap().is_none());
    }

    #[test]
    fn test_list_index_pages_reports_freshness() {
        let (_dir, meta) = store();
        meta.put_index("pypi/flask", &record()).unwrap();
        meta.put_index(
            "pypi/numpy",
            &CachedIndex {
                fetched_at_unix: 1_800_000_000,
                fresh_secs: Some(600),
                ..record()
            },
        )
        .unwrap();
        let mut pages = meta.list_index_pages().unwrap();
        pages.sort();
        assert_eq!(
            pages,
            vec![
                ("pypi/flask".to_owned(), 1_700_000_000, None),
                ("pypi/numpy".to_owned(), 1_800_000_000, Some(600)),
            ]
        );
    }

    #[test]
    fn test_list_index_pages_reads_a_legacy_plain_json_record() {
        let (_dir, meta) = store();
        // A record written by a version that stored the whole struct as plain JSON, not the framed form.
        let legacy = serde_json::to_vec(&record()).unwrap();
        meta.put_driver_value(&index_key("pypi/old"), &legacy).unwrap();
        assert_eq!(
            meta.list_index_pages().unwrap(),
            vec![("pypi/old".to_owned(), 1_700_000_000, None)]
        );
    }

    #[test]
    fn test_scan_index_pages_visits_records_without_collecting() {
        let (_dir, meta) = store();
        meta.put_index("pypi/flask", &record()).unwrap();
        let mut pages = Vec::new();
        meta.scan_index_pages(|page| {
            pages.push((page.key, page.summary.body_bytes));
            Ok::<(), std::io::Error>(())
        })
        .unwrap();
        assert_eq!(pages, vec![("pypi/flask".to_owned(), 13)]);
    }

    #[test]
    fn test_scan_index_pages_reports_the_visitor_error_source() {
        let (_dir, meta) = store();
        meta.put_index("pypi/flask", &record()).unwrap();
        let err = meta
            .scan_index_pages(|_page| Err(std::io::Error::other("stop")))
            .unwrap_err();
        assert_eq!(err.to_string(), "stop");
        assert!(err.source().is_some());
    }

    #[test]
    fn test_scan_index_records_visits_raw_bytes() {
        let (_dir, meta) = store();
        meta.put_index("pypi/flask", &record()).unwrap();
        let mut keys = Vec::new();
        meta.scan_index_records(|key, raw| {
            keys.push((key.to_owned(), raw.starts_with(b"peryx1\n")));
            Ok::<(), std::io::Error>(())
        })
        .unwrap();
        assert_eq!(keys, vec![("pypi/flask".to_owned(), true)]);
    }
}
