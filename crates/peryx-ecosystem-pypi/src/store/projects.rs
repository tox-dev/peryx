use std::collections::BTreeMap;

use peryx_storage::meta::{DriverBatch, MetaError, MetaScanError, MetaStore};
use serde::{Deserialize, Serialize};

use super::{
    CATALOG_GENERATION_PREFIX, CATALOG_PREFIX, PROJECTS_PREFIX, file_key, freshness_key, index_key, metadata_key,
    project_key, project_status_key,
};

const CATALOG_DELETE_BATCH: usize = 10_000;

/// One completely parsed remote root catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogGeneration {
    pub generation: u64,
    pub source: String,
    pub url: String,
    pub format: String,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub last_serial: Option<u64>,
    pub fetched_at_unix: i64,
    pub bytes: u64,
    pub projects: u64,
}

/// Publication state for one cached index's remote root catalog.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogState {
    pub active: Option<CatalogGeneration>,
    pub staging: Option<u64>,
    pub retired: Option<u64>,
    pub next_generation: u64,
}

fn catalog_key(index: &str) -> String {
    format!("{CATALOG_PREFIX}{index}")
}

fn catalog_generation_prefix(index: &str, generation: u64) -> String {
    format!("{CATALOG_GENERATION_PREFIX}{index}/{generation:020}/")
}

fn catalog_project_key(index: &str, generation: u64, normalized: &str) -> String {
    format!("{}{normalized}", catalog_generation_prefix(index, generation))
}

fn decode_catalog_state(raw: Option<Vec<u8>>) -> Result<CatalogState, MetaError> {
    raw.map_or_else(|| Ok(CatalogState::default()), |raw| Ok(serde_json::from_slice(&raw)?))
}

/// Read the currently published catalog metadata.
///
/// # Errors
/// Returns a store error if the read or decode fails.
pub fn catalog_state(meta: &MetaStore, index: &str) -> Result<CatalogState, MetaError> {
    decode_catalog_state(meta.get_driver_value(&catalog_key(index))?)
}

fn store_catalog_state(
    txn: &mut peryx_storage::meta::DriverTxn<'_>,
    index: &str,
    state: &CatalogState,
) -> Result<(), MetaError> {
    txn.put_local(&catalog_key(index), &serde_json::to_vec(state)?)
}

/// Remove generations left by an interrupted sync and clear their state only after all rows are gone.
///
/// # Errors
/// Returns a store error if a read, deletion, or state update fails.
pub fn recover_catalog_generations(meta: &MetaStore, index: &str) -> Result<(), MetaError> {
    let state = catalog_state(meta, index)?;
    for generation in [state.staging, state.retired].into_iter().flatten() {
        let prefix = catalog_generation_prefix(index, generation);
        loop {
            let keys = meta.driver_prefix_keys_limited(&prefix, CATALOG_DELETE_BATCH)?;
            if keys.is_empty() {
                break;
            }
            let mut batch = DriverBatch::new();
            for key in keys {
                batch.delete(key);
            }
            meta.commit_driver_batch(&batch, false)?;
        }
    }
    meta.commit_driver_txn(|txn| {
        let mut current = decode_catalog_state(txn.get(&catalog_key(index))?)?;
        if current.staging == state.staging {
            current.staging = None;
        }
        if current.retired == state.retired {
            current.retired = None;
        }
        store_catalog_state(txn, index, &current)?;
        Ok::<_, MetaError>(((), Vec::new()))
    })
}

/// Atomically reserve the next generation and return it with the active generation expected at publication.
///
/// # Errors
/// Returns a store error if the reservation fails.
pub fn begin_catalog_generation(meta: &MetaStore, index: &str) -> Result<(u64, Option<u64>), MetaError> {
    meta.commit_driver_txn(|txn| {
        let mut state = decode_catalog_state(txn.get(&catalog_key(index))?)?;
        let expected = state.active.as_ref().map(|active| active.generation);
        state.next_generation += 1;
        state.staging = Some(state.next_generation);
        store_catalog_state(txn, index, &state)?;
        Ok::<_, MetaError>(((state.next_generation, expected), Vec::new()))
    })
}

/// Add a bounded batch of canonical/display pairs to a staging generation.
///
/// Duplicate canonical names retain the bytewise-smallest display spelling, making the result
/// independent of upstream ordering. Returns the number of newly inserted canonical names.
///
/// # Errors
/// Returns a store error if the transaction fails.
pub fn put_catalog_projects(
    meta: &MetaStore,
    index: &str,
    generation: u64,
    projects: &[(String, String)],
) -> Result<u64, MetaError> {
    meta.commit_driver_txn(|txn| {
        let state = decode_catalog_state(txn.get(&catalog_key(index))?)?;
        if state.staging != Some(generation) {
            return Err(MetaError::DriverPrecondition(
                "catalog generation is not staging".to_owned(),
            ));
        }
        let mut inserted = 0;
        for (normalized, display) in projects {
            let key = catalog_project_key(index, generation, normalized);
            match txn.get(&key)? {
                None => {
                    txn.put_local(&key, display.as_bytes())?;
                    inserted += 1;
                }
                Some(current) if display.as_bytes() < current.as_slice() => txn.put_local(&key, display.as_bytes())?,
                Some(_) => {}
            }
        }
        Ok::<_, MetaError>((inserted, Vec::new()))
    })
}

/// Publish a fully parsed generation if both the staging reservation and active generation still match.
///
/// # Errors
/// Returns a store error if publication loses its compare-and-swap or the transaction fails.
pub fn publish_catalog_generation(
    meta: &MetaStore,
    index: &str,
    expected_active: Option<u64>,
    generation: CatalogGeneration,
) -> Result<(), MetaError> {
    meta.commit_driver_txn_with_catalog_generation(index, generation.generation, |txn| {
        let mut state = decode_catalog_state(txn.get(&catalog_key(index))?)?;
        if state.staging != Some(generation.generation)
            || state.active.as_ref().map(|active| active.generation) != expected_active
        {
            return Err(MetaError::DriverPrecondition(
                "catalog publication lost its reservation".to_owned(),
            ));
        }
        state.retired = state.active.as_ref().map(|active| active.generation);
        state.active = Some(generation);
        state.staging = None;
        store_catalog_state(txn, index, &state)?;
        Ok::<_, MetaError>(((), Vec::new()))
    })
}

/// Discard one failed staging generation without disturbing a newer reservation.
///
/// # Errors
/// Returns a store error if row cleanup or the state update fails.
pub fn abort_catalog_generation(meta: &MetaStore, index: &str, generation: u64) -> Result<(), MetaError> {
    let prefix = catalog_generation_prefix(index, generation);
    loop {
        let keys = meta.driver_prefix_keys_limited(&prefix, CATALOG_DELETE_BATCH)?;
        if keys.is_empty() {
            break;
        }
        let mut batch = DriverBatch::new();
        for key in keys {
            batch.delete(key);
        }
        meta.commit_driver_batch(&batch, false)?;
    }
    meta.commit_driver_txn(|txn| {
        let mut state = decode_catalog_state(txn.get(&catalog_key(index))?)?;
        if state.staging == Some(generation) {
            state.staging = None;
            store_catalog_state(txn, index, &state)?;
        }
        Ok::<_, MetaError>(((), Vec::new()))
    })
}

/// Refresh a published generation after a `304`, merging only validators present in the response.
///
/// # Errors
/// Returns a store error if the active generation changed or the transaction fails.
pub fn refresh_catalog_generation(
    meta: &MetaStore,
    index: &str,
    expected: u64,
    etag: Option<String>,
    last_modified: Option<String>,
    fetched_at_unix: i64,
) -> Result<(), MetaError> {
    meta.commit_driver_txn(|txn| {
        let mut state = decode_catalog_state(txn.get(&catalog_key(index))?)?;
        let active = state
            .active
            .as_mut()
            .filter(|active| active.generation == expected)
            .ok_or_else(|| MetaError::DriverPrecondition("catalog changed during revalidation".to_owned()))?;
        if etag.is_some() {
            active.etag = etag;
        }
        if last_modified.is_some() {
            active.last_modified = last_modified;
        }
        active.fetched_at_unix = fetched_at_unix;
        store_catalog_state(txn, index, &state)?;
        Ok::<_, MetaError>(((), Vec::new()))
    })
}

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
    let mut local = BTreeMap::new();
    meta.visit_driver_prefix(&prefix, |key, raw| {
        if let Ok(display) = std::str::from_utf8(raw) {
            local.insert(key[prefix.len()..].to_owned(), display.to_owned());
        }
    })?;
    let mut names = Vec::new();
    if let Some(active) = catalog_state(meta, index)?.active {
        let catalog_prefix = catalog_generation_prefix(index, active.generation);
        meta.visit_driver_prefix(&catalog_prefix, |key, raw| {
            if let Some(display) = local.remove(&key[catalog_prefix.len()..]) {
                names.push(display);
            } else if let Ok(display) = std::str::from_utf8(raw) {
                names.push(display.to_owned());
            }
        })?;
    }
    names.extend(local.into_values());
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
    use super::{
        CatalogGeneration, MetaStore, ProjectCachePurgeCounts, abort_catalog_generation, begin_catalog_generation,
        catalog_generation_prefix, catalog_state, freshness_key, project_key, publish_catalog_generation,
        put_catalog_projects, recover_catalog_generations, refresh_catalog_generation,
    };
    use crate::store::PypiStore as _;

    fn store() -> (tempfile::TempDir, MetaStore) {
        let dir = tempfile::tempdir().unwrap();
        let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
        (dir, meta)
    }

    fn generation(generation: u64, etag: Option<&str>, last_modified: Option<&str>) -> CatalogGeneration {
        CatalogGeneration {
            generation,
            source: "pypi".to_owned(),
            url: "https://pypi.org/simple/".to_owned(),
            format: "json".to_owned(),
            etag: etag.map(str::to_owned),
            last_modified: last_modified.map(str::to_owned),
            last_serial: Some(7),
            fetched_at_unix: 1,
            bytes: 100,
            projects: 2,
        }
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
    fn test_catalog_duplicates_are_order_independent_and_local_display_wins() {
        for projects in [
            [
                ("flask".to_owned(), "flask".to_owned()),
                ("flask".to_owned(), "Flask".to_owned()),
            ],
            [
                ("flask".to_owned(), "Flask".to_owned()),
                ("flask".to_owned(), "flask".to_owned()),
            ],
        ] {
            let (_dir, meta) = store();
            let (id, expected) = begin_catalog_generation(&meta, "pypi").unwrap();
            assert_eq!(put_catalog_projects(&meta, "pypi", id, &projects).unwrap(), 1);
            publish_catalog_generation(&meta, "pypi", expected, generation(id, None, None)).unwrap();
            assert_eq!(meta.list_projects("pypi").unwrap(), vec!["Flask"]);
            meta.put_project("pypi", "flask", "Local-Flask").unwrap();
            assert_eq!(meta.list_projects("pypi").unwrap(), vec!["Local-Flask"]);
        }
    }

    #[test]
    fn test_catalog_abort_and_stale_publish_only_touch_their_generation() {
        let (_dir, meta) = store();
        let (first, expected) = begin_catalog_generation(&meta, "pypi").unwrap();
        put_catalog_projects(&meta, "pypi", first, &[("first".to_owned(), "first".to_owned())]).unwrap();
        let (second, _) = begin_catalog_generation(&meta, "pypi").unwrap();
        put_catalog_projects(&meta, "pypi", second, &[("second".to_owned(), "second".to_owned())]).unwrap();

        abort_catalog_generation(&meta, "pypi", first).unwrap();

        assert_eq!(catalog_state(&meta, "pypi").unwrap().staging, Some(second));
        assert!(
            meta.driver_prefix_keys(&catalog_generation_prefix("pypi", first))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            meta.driver_prefix_keys(&catalog_generation_prefix("pypi", second))
                .unwrap()
                .len(),
            1
        );
        assert!(publish_catalog_generation(&meta, "pypi", expected, generation(first, None, None)).is_err());
    }

    #[test]
    fn test_catalog_batch_requires_its_staging_generation() {
        let (_dir, meta) = store();

        let error = put_catalog_projects(&meta, "pypi", 1, &[("flask".to_owned(), "Flask".to_owned())]).unwrap_err();

        assert!(matches!(error, peryx_storage::meta::MetaError::DriverPrecondition(_)));
    }

    #[test]
    fn test_catalog_refresh_merges_present_validators() {
        let (_dir, meta) = store();
        let (id, expected) = begin_catalog_generation(&meta, "pypi").unwrap();
        publish_catalog_generation(
            &meta,
            "pypi",
            expected,
            generation(id, Some("old-etag"), Some("old-date")),
        )
        .unwrap();

        refresh_catalog_generation(&meta, "pypi", id, None, Some("new-date".to_owned()), 9).unwrap();
        assert!(refresh_catalog_generation(&meta, "pypi", id + 1, None, None, 10).is_err());

        let active = catalog_state(&meta, "pypi").unwrap().active.unwrap();
        assert_eq!(active.etag.as_deref(), Some("old-etag"));
        assert_eq!(active.last_modified.as_deref(), Some("new-date"));
        assert_eq!(active.fetched_at_unix, 9);
    }

    #[test]
    fn test_catalog_recovery_preserves_active_and_removes_pending_generations() {
        let (_dir, meta) = store();
        let (first, expected) = begin_catalog_generation(&meta, "pypi").unwrap();
        put_catalog_projects(&meta, "pypi", first, &[("first".to_owned(), "first".to_owned())]).unwrap();
        publish_catalog_generation(&meta, "pypi", expected, generation(first, None, None)).unwrap();
        let (second, expected) = begin_catalog_generation(&meta, "pypi").unwrap();
        put_catalog_projects(&meta, "pypi", second, &[("second".to_owned(), "second".to_owned())]).unwrap();
        publish_catalog_generation(&meta, "pypi", expected, generation(second, None, None)).unwrap();
        let (third, _) = begin_catalog_generation(&meta, "pypi").unwrap();
        put_catalog_projects(&meta, "pypi", third, &[("third".to_owned(), "third".to_owned())]).unwrap();

        recover_catalog_generations(&meta, "pypi").unwrap();

        let state = catalog_state(&meta, "pypi").unwrap();
        assert_eq!(state.active.unwrap().generation, second);
        assert_eq!(state.staging, None);
        assert_eq!(state.retired, None);
        assert!(
            meta.driver_prefix_keys(&catalog_generation_prefix("pypi", first))
                .unwrap()
                .is_empty()
        );
        assert!(
            meta.driver_prefix_keys(&catalog_generation_prefix("pypi", third))
                .unwrap()
                .is_empty()
        );
        assert_eq!(meta.list_projects("pypi").unwrap(), vec!["second"]);
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
