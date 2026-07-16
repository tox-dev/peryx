use peryx_storage::meta::{DriverBatch, MetaError, MetaScanError, MetaStore};

use super::{PROJECTS_PREFIX, file_key, freshness_key, index_key, metadata_key, project_key, project_status_key};

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
        if let Some(value) = meta.get_driver_value(&key)?.and_then(|raw| String::from_utf8(raw).ok()) {
            visit(&key[PROJECTS_PREFIX.len()..], &value).map_err(MetaScanError::Visit)?;
        }
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
    batch.delete(freshness_key(&key));
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

#[cfg(test)]
mod tests {
    use super::{MetaStore, ProjectCachePurgeCounts, freshness_key, project_key};
    use crate::store::PypiStore as _;

    fn store() -> (tempfile::TempDir, MetaStore) {
        let dir = tempfile::tempdir().unwrap();
        let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
        (dir, meta)
    }

    #[test]
    fn test_put_and_list_projects_are_sorted_and_deduplicated() {
        let (_dir, meta) = store();
        assert!(meta.list_projects("root/pypi").unwrap().is_empty());
        meta.put_project("root/pypi", "flask", "Flask").unwrap();
        meta.put_project("root/pypi", "django", "Django").unwrap();
        meta.put_project("other/index", "x", "X").unwrap();
        meta.put_project("root/pypi", "flask", "Flask").unwrap();
        assert_eq!(meta.list_projects("root/pypi").unwrap(), vec!["Django", "Flask"]);
        assert_eq!(
            meta.get_project("root/pypi", "flask").unwrap().as_deref(),
            Some("Flask")
        );
    }

    #[test]
    fn test_count_then_delete_project_cache_reports_and_removes_each_row() {
        let (_dir, meta) = store();
        let record = crate::store::CachedIndex {
            etag: None,
            last_serial: None,
            fetched_at_unix: 1,
            content_type: None,
            fresh_secs: None,
            body: Vec::new(),
        };
        let file_digests = vec!["a".repeat(64)];
        let metadata_digests = vec!["b".repeat(64)];
        meta.put_cached_page(
            "pypi/flask",
            &record,
            "pypi",
            "flask",
            "Flask",
            "pypi",
            None,
            Some("archived"),
            Some("read only"),
            &[(file_digests[0].clone(), "https://files/flask.whl".to_owned(), Some(123))],
            &[(
                metadata_digests[0].clone(),
                "https://files/flask.whl.metadata".to_owned(),
                "c".repeat(64),
            )],
        )
        .unwrap();

        let expected = ProjectCachePurgeCounts {
            index_pages: 1,
            project_records: 1,
            project_status_records: 1,
            file_url_records: 1,
            metadata_records: 1,
        };
        assert_eq!(
            meta.count_project_cache_purge("pypi", "flask", &file_digests, &metadata_digests)
                .unwrap(),
            expected
        );
        assert_eq!(
            meta.delete_project_cache("pypi", "flask", &file_digests, &metadata_digests)
                .unwrap(),
            expected
        );
        assert!(meta.get_index("pypi/flask").unwrap().is_none());
        assert!(meta.get_file_url(&file_digests[0]).unwrap().is_none());
        assert!(meta.get_metadata(&metadata_digests[0]).unwrap().is_none());
        assert!(meta.get_project_status("pypi", "flask").unwrap().is_none());
        assert!(meta.list_projects("pypi").unwrap().is_empty());
    }

    #[test]
    fn test_delete_project_cache_removes_the_freshness_overlay() {
        let (_dir, meta) = store();
        let record = crate::store::CachedIndex {
            etag: None,
            last_serial: None,
            fetched_at_unix: 1,
            content_type: None,
            fresh_secs: None,
            body: Vec::new(),
        };
        meta.put_index("pypi/flask", &record).unwrap();
        meta.touch_index_freshness("pypi/flask", 42, Some(9)).unwrap();
        assert!(meta.get_driver_value(&freshness_key("pypi/flask")).unwrap().is_some());

        meta.delete_project_cache("pypi", "flask", &[], &[]).unwrap();

        assert!(meta.get_driver_value(&freshness_key("pypi/flask")).unwrap().is_none());
    }

    #[test]
    fn test_scan_project_records_visits_valid_and_skips_non_utf8() {
        let (_dir, meta) = store();
        meta.put_project("pypi", "flask", "Flask").unwrap();
        meta.put_driver_value(&project_key("pypi", "bad"), &[0xff, 0xfe])
            .unwrap();
        let mut seen = Vec::new();
        meta.scan_project_records(|key, value| {
            seen.push((key.to_owned(), value.to_owned()));
            Ok::<(), std::io::Error>(())
        })
        .unwrap();
        assert_eq!(seen, vec![("pypi/flask".to_owned(), "Flask".to_owned())]);
    }
}
