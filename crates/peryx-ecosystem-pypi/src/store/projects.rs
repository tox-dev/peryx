use std::collections::BTreeMap;

use peryx_storage::meta::{DriverBatch, MetaError, MetaScanError, MetaStore};
use serde::{Deserialize, Serialize};

use super::{
    CATALOG_GENERATION_PREFIX, CATALOG_PREFIX, PROJECTS_PREFIX, file_key, freshness_key, index_key, metadata_key,
    project_key, project_status_key, publication_prefix,
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

/// # Errors
/// Returns a store error if the read fails.
pub fn get_project(meta: &MetaStore, index: &str, normalized: &str) -> Result<Option<String>, MetaError> {
    Ok(meta
        .get_driver_value(&project_key(index, normalized))?
        .and_then(|raw| String::from_utf8(raw).ok()))
}

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

/// # Errors
/// Returns a store error if the read fails.
pub fn list_projects(meta: &MetaStore, index: &str) -> Result<Vec<String>, MetaError> {
    meta.read_driver_txn(|txn| {
        let prefix = format!("{PROJECTS_PREFIX}{index}/");
        let catalog_prefix = decode_catalog_state(txn.get(&catalog_key(index))?)?
            .active
            .map(|active| catalog_generation_prefix(index, active.generation));
        let prefixes: &[&str] = match &catalog_prefix {
            Some(catalog_prefix) => &[&prefix, catalog_prefix],
            None => &[&prefix],
        };
        let mut local = BTreeMap::new();
        let mut names = Vec::new();
        for (group_prefix, entries) in prefixes.iter().zip(txn.prefixes(prefixes)?) {
            for (key, raw) in entries {
                if *group_prefix == prefix {
                    if let Ok(display) = std::str::from_utf8(&raw) {
                        local.insert(key[prefix.len()..].to_owned(), display.to_owned());
                    }
                } else if let Some(display) = local.remove(&key[group_prefix.len()..]) {
                    names.push(display);
                } else if let Ok(display) = std::str::from_utf8(&raw) {
                    names.push(display.to_owned());
                }
            }
        }
        names.extend(local.into_values());
        names.sort();
        Ok(names)
    })
}

/// # Errors
/// Returns a store error if the catalog state or project-key scan cannot be read.
pub fn list_catalog_projects(meta: &MetaStore, index: &str, limit: usize) -> Result<Vec<String>, MetaError> {
    let Some(active) = catalog_state(meta, index)?.active else {
        return Ok(Vec::new());
    };
    let prefix = catalog_generation_prefix(index, active.generation);
    Ok(meta
        .driver_prefix_keys_limited(&prefix, limit)?
        .into_iter()
        .map(|key| key[prefix.len()..].to_owned())
        .collect())
}

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
    for key in meta.driver_prefix_keys(&publication_prefix(index, normalized))? {
        batch.delete(key);
    }
    meta.commit_driver_batch(&batch, true)?;
    Ok(counts)
}

#[cfg(test)]
#[path = "../../tests/unit/store/projects/tests.rs"]
mod tests;
