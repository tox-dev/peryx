use peryx_storage::meta::{DriverBatch, DriverReadTxn, DriverTxn, MetaError, MetaScanError, MetaStore};

use crate::simple::File;
use crate::stream::metadata_sibling;
use crate::{CoreMetadata, to_json};

use super::attestations::{
    publish_staged_upstream_attestations_in_txn, replace_project_upstream_attestations_in_txn,
    stage_upstream_attestation_in_txn,
};
use super::record::{
    CachedIndex, CachedIndexPage, FreshnessOverlay, ProjectGeneration, ProjectMetaState, ProjectStatusRecord,
};
use super::{
    INDEX_PREFIX, UpstreamAttestation, file_key, file_source_value, freshness_key, index_key, project_status_key,
    publication_key, publication_prefix, publication_value, put_cached_project_row,
};
use super::{project_file_key, project_generation_attestation_prefix, project_generation_prefix, project_meta_key};

/// How many generation rows a purge deletes per transaction, bounding one commit for a project with
/// a very large file list.
const PROJECT_FILE_DELETE_BATCH: usize = 10_000;

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
                    txn.put_local(&file_key(&file.sha256), value.as_bytes()).and_then(|()| {
                        let key = publication_key(index, normalized, &file.sha256, &file.filename);
                        let value = publication_value(file.metadata.as_ref(), source, upstream);
                        txn.put_local(&key, value.as_bytes())
                    })
                })
            })
            .and_then(|()| replace_project_upstream_attestations_in_txn(txn, index, normalized, upstream, attestations))
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

fn decode_project_meta_state(raw: Option<Vec<u8>>) -> Result<ProjectMetaState, MetaError> {
    raw.map_or_else(
        || Ok(ProjectMetaState::default()),
        |raw| Ok(serde_json::from_slice(&raw)?),
    )
}

/// # Errors
/// Returns a store error if the read or decode fails.
pub fn project_meta_state(meta: &MetaStore, index: &str, normalized: &str) -> Result<ProjectMetaState, MetaError> {
    decode_project_meta_state(meta.get_driver_value(&project_meta_key(index, normalized))?)
}

/// # Errors
/// Returns a store error if the read or decode fails.
pub fn active_project_generation(
    meta: &MetaStore,
    index: &str,
    normalized: &str,
) -> Result<Option<ProjectGeneration>, MetaError> {
    Ok(project_meta_state(meta, index, normalized)?.active)
}

fn store_project_meta_state(
    txn: &mut DriverTxn<'_>,
    index: &str,
    normalized: &str,
    state: &ProjectMetaState,
) -> Result<(), MetaError> {
    txn.put_local(&project_meta_key(index, normalized), &serde_json::to_vec(state)?)
}

fn delete_generation_rows(meta: &MetaStore, index: &str, normalized: &str, generation: u64) -> Result<(), MetaError> {
    [
        project_generation_prefix(index, normalized, generation),
        project_generation_attestation_prefix(index, normalized, generation),
    ]
    .into_iter()
    .try_for_each(|prefix| delete_generation_prefix(meta, &prefix))
}

fn delete_generation_prefix(meta: &MetaStore, prefix: &str) -> Result<(), MetaError> {
    meta.driver_prefix_keys_limited(prefix, PROJECT_FILE_DELETE_BATCH)
        .and_then(|keys| {
            if keys.is_empty() {
                return Ok(());
            }
            let mut batch = DriverBatch::new();
            for key in keys {
                batch.delete(key);
            }
            meta.commit_driver_batch(&batch, false)
                .and_then(|()| delete_generation_prefix(meta, prefix))
        })
}

/// Remove generations left by an interrupted sync, clearing their state only after every row is gone.
///
/// # Errors
/// Returns a store error if a read, deletion, or state update fails.
pub fn recover_project_generations(meta: &MetaStore, index: &str, normalized: &str) -> Result<(), MetaError> {
    let state = project_meta_state(meta, index, normalized)?;
    for generation in [state.staging, state.retired].into_iter().flatten() {
        delete_generation_rows(meta, index, normalized, generation)?;
    }
    meta.commit_driver_txn(|txn| {
        let mut current = decode_project_meta_state(txn.get(&project_meta_key(index, normalized))?)?;
        if current.staging == state.staging {
            current.staging = None;
        }
        if current.retired == state.retired {
            current.retired = None;
        }
        store_project_meta_state(txn, index, normalized, &current)?;
        Ok::<_, MetaError>(((), Vec::new()))
    })
}

/// Reserve the next generation for one project and return it with the active generation expected at
/// publication, so a concurrent sync cannot silently overwrite a newer one.
///
/// # Errors
/// Returns a store error if the reservation fails.
pub fn begin_project_generation(
    meta: &MetaStore,
    index: &str,
    normalized: &str,
) -> Result<(u64, Option<u64>), MetaError> {
    meta.commit_driver_txn(|txn| {
        let mut state = decode_project_meta_state(txn.get(&project_meta_key(index, normalized))?)?;
        let expected = state.active.as_ref().map(|active| active.generation);
        state.next_generation += 1;
        state.staging = Some(state.next_generation);
        store_project_meta_state(txn, index, normalized, &state)?;
        Ok::<_, MetaError>(((state.next_generation, expected), Vec::new()))
    })
}

/// Add a bounded batch of parsed remote files to a staging generation.
///
/// Each admitted file's download source and PEP 658 sibling are registered so a cache hit resolves by
/// digest. The first spelling of a duplicate filename wins, making the result independent of upstream
/// ordering. Returns the number of newly inserted filenames.
///
/// # Errors
/// Returns a store error if the transaction fails or the generation is no longer staging.
pub fn put_project_files(
    meta: &MetaStore,
    index: &str,
    normalized: &str,
    generation: u64,
    source: &str,
    upstream: Option<&str>,
    files: &[File],
) -> Result<u64, MetaError> {
    meta.commit_driver_txn(|txn| {
        txn.get(&project_meta_key(index, normalized))
            .and_then(decode_project_meta_state)
            .and_then(|state| {
                if state.staging == Some(generation) {
                    Ok(())
                } else {
                    Err(MetaError::DriverPrecondition(
                        "project generation is not staging".to_owned(),
                    ))
                }
            })
            .and_then(|()| {
                files.iter().try_fold(0, |inserted, file| {
                    let key = project_file_key(index, normalized, generation, &file.filename);
                    txn.get(&key).and_then(|current| {
                        if current.is_some() {
                            return Ok(inserted);
                        }
                        txn.put_local(&key, to_json(file).as_bytes())
                            .and_then(|()| {
                                register_file_rows(txn, index, normalized, generation, source, upstream, file)
                            })
                            .map(|()| inserted + 1)
                    })
                })
            })
            .map(|inserted| (inserted, Vec::new()))
    })
}

fn register_file_rows(
    txn: &mut DriverTxn<'_>,
    index: &str,
    project: &str,
    generation: u64,
    source: &str,
    upstream: Option<&str>,
    file: &File,
) -> Result<(), MetaError> {
    let Some(sha256) = file.sha256() else {
        return Ok(());
    };
    let source_value = file_source_value(&file.url, source, file.size, upstream);
    txn.put_local(&file_key(sha256), source_value.as_bytes())?;
    let claim = match file.metadata() {
        CoreMetadata::Hashes(hashes) => hashes
            .get("sha256")
            .map(|digest| (metadata_sibling(&file.url), digest.clone())),
        CoreMetadata::Absent | CoreMetadata::Available => None,
    };
    let key = publication_key(index, project, sha256, &file.filename);
    let publication = publication_value(claim.as_ref(), source, upstream);
    txn.put_local(&key, publication.as_bytes())?;
    file.provenance.secure_url().map_or(Ok(()), |url| {
        let record = UpstreamAttestation::remote(url, index, project, upstream);
        stage_upstream_attestation_in_txn(txn, index, generation, sha256, &file.filename, &record)
    })
}

/// Visit the file rows every project generation holds, keyed relative to the namespace as
/// `{index}/{project}/{generation}/{filename}`.
///
/// A cached project that a catalog sync populated keeps its files here rather than in a cached page
/// body, so a caller enumerating what the store still refers to has to read both.
///
/// # Errors
/// Returns a scan error if the store read fails or the visitor returns an error.
pub fn scan_project_file_records<E>(
    meta: &MetaStore,
    mut visit: impl FnMut(&str, &[u8]) -> Result<(), E>,
) -> Result<(), MetaScanError<E>> {
    let mut error = None;
    meta.visit_driver_prefix(super::PROJECT_FILE_PREFIX, |key, record| {
        if error.is_none() {
            error = visit(&key[super::PROJECT_FILE_PREFIX.len()..], record).err();
        }
    })?;
    if let Some(err) = error {
        return Err(MetaScanError::Visit(err));
    }
    Ok(())
}

/// Publish a fully parsed generation, swapping the active pointer only if both the staging
/// reservation and the active generation still match what the sync observed.
///
/// # Errors
/// Returns a store error if publication loses its compare-and-swap or the transaction fails.
pub fn publish_project_generation(
    meta: &MetaStore,
    index: &str,
    normalized: &str,
    expected_active: Option<u64>,
    generation: ProjectGeneration,
) -> Result<(), MetaError> {
    meta.commit_driver_txn(|txn| {
        let mut state = decode_project_meta_state(txn.get(&project_meta_key(index, normalized))?)?;
        if state.staging != Some(generation.generation)
            || state.active.as_ref().map(|active| active.generation) != expected_active
        {
            return Err(MetaError::DriverPrecondition(
                "project publication lost its reservation".to_owned(),
            ));
        }
        let published = generation.generation;
        state.retired = state.active.as_ref().map(|active| active.generation);
        state.active = Some(generation);
        state.staging = None;
        publish_staged_upstream_attestations_in_txn(txn, index, normalized, published)
            .and_then(|()| store_project_meta_state(txn, index, normalized, &state))
            .map(|()| ((), Vec::new()))
    })
}

/// Discard one failed staging generation without disturbing a newer reservation.
///
/// # Errors
/// Returns a store error if row cleanup or the state update fails.
pub fn abort_project_generation(
    meta: &MetaStore,
    index: &str,
    normalized: &str,
    generation: u64,
) -> Result<(), MetaError> {
    delete_generation_rows(meta, index, normalized, generation)?;
    meta.commit_driver_txn(|txn| {
        let mut state = decode_project_meta_state(txn.get(&project_meta_key(index, normalized))?)?;
        if state.staging == Some(generation) {
            state.staging = None;
            store_project_meta_state(txn, index, normalized, &state)?;
        }
        Ok::<_, MetaError>(((), Vec::new()))
    })
}

/// Refresh the active generation after a `304 Not Modified`, merging only validators the response
/// carried and advancing the observation time, without touching the file rows.
///
/// # Errors
/// Returns a store error if the active generation changed or the transaction fails.
pub fn refresh_project_generation(
    meta: &MetaStore,
    index: &str,
    normalized: &str,
    expected: u64,
    etag: Option<String>,
    last_modified: Option<String>,
    fetched_at_unix: i64,
) -> Result<(), MetaError> {
    meta.commit_driver_txn(|txn| {
        let mut state = decode_project_meta_state(txn.get(&project_meta_key(index, normalized))?)?;
        let active = state
            .active
            .as_mut()
            .filter(|active| active.generation == expected)
            .ok_or_else(|| MetaError::DriverPrecondition("project changed during revalidation".to_owned()))?;
        if etag.is_some() {
            active.etag = etag;
        }
        if last_modified.is_some() {
            active.last_modified = last_modified;
        }
        active.fetched_at_unix = fetched_at_unix;
        store_project_meta_state(txn, index, normalized, &state)?;
        Ok::<_, MetaError>(((), Vec::new()))
    })
}

/// # Errors
/// Returns a store error if a read fails or a stored file row cannot be decoded.
pub fn list_project_files(meta: &MetaStore, index: &str, normalized: &str) -> Result<Vec<File>, MetaError> {
    meta.read_driver_txn(|txn| project_files_in_snapshot(txn, index, normalized))
}

/// Publication retires the active generation and a later sync reclaims its rows in bounded batches,
/// so a pointer read and a row scan taken from two snapshots can name a generation whose rows are
/// already partly or wholly gone. One snapshot is what keeps the answer a whole generation.
fn project_files_in_snapshot(txn: &DriverReadTxn, index: &str, normalized: &str) -> Result<Vec<File>, MetaError> {
    let Some(active) = decode_project_meta_state(txn.get(&project_meta_key(index, normalized))?)?.active else {
        return Ok(Vec::new());
    };
    txn.prefix(&project_generation_prefix(index, normalized, active.generation))?
        .into_iter()
        .map(|(_key, raw)| serde_json::from_slice(&raw).map_err(MetaError::from))
        .collect()
}

#[cfg(test)]
#[path = "../../tests/unit/store/index/tests.rs"]
mod tests;
