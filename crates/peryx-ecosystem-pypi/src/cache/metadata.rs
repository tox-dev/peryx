//! PEP 658 metadata resolution: cached sidecars, ranged wheel reads, and background backfill.

use std::io::{Cursor, Read as _};
use std::sync::{Arc, Mutex};

use crate::store::PypiStore as _;
use crate::stream::Registration;
use bytes::Bytes;
use peryx_driver::state::ServingState;
use peryx_storage::blob::Digest;
use peryx_upstream::{ArtifactClient, RangeError};

mod central_dir;
use central_dir::{
    DirectoryEntrySearch, ZIP_COMPRESSION_DEFLATED, ZIP_COMPRESSION_STORED, ZIP_LOCAL_SIGNATURE, ZIP_TAIL_BYTES,
    central_directory, find_central_directory_entry, read_u16,
};

use super::download::file_path;
use super::{
    CacheError, NEGATIVE_TTL_SECS, ensure_digest_clear, is_tar_gz, is_wheel, source_artifact_client, source_client,
    upstream_permit,
};

const _: () = assert!(crate::archive::MAX_WHEEL_METADATA_BYTES <= usize::MAX as u64);

async fn fetch_from_source(
    state: &ServingState,
    source: &str,
    upstream: Option<&str>,
    url: &str,
) -> Result<Bytes, CacheError> {
    let (client, offline) = source_client(state, source, upstream)?;
    if offline {
        return Err(CacheError::OfflineMissing("metadata"));
    }
    let _permit = upstream_permit(state, source).await?;
    Ok(client
        .fetch_bytes_limited(
            url,
            usize::try_from(crate::archive::MAX_WHEEL_METADATA_BYTES).expect("metadata limit fits in memory"),
        )
        .await?)
}

/// Resolve an artifact's PEP 658 metadata bytes: cached blob, advertised upstream sibling, or
/// generated metadata extracted from the artifact.
///
/// # Errors
/// Returns [`CacheError::FileNotFound`] if the artifact has no usable metadata source, or another
/// error on a store, archive, or upstream failure.
pub async fn metadata_bytes(
    state: &Arc<ServingState>,
    artifact_digest: &Digest,
    route: &str,
    metadata_filename: &str,
) -> Result<Bytes, CacheError> {
    ensure_digest_clear(state, artifact_digest)?;
    let artifact_filename = metadata_filename
        .strip_suffix(".metadata")
        .ok_or(CacheError::FileNotFound)?;
    let negative_key = metadata_negative_key(artifact_digest);
    if state.negative_fresh(&negative_key) {
        return Err(CacheError::FileNotFound);
    }
    if let Some((url, metadata_hex, source)) = state.meta.get_metadata(artifact_digest.as_str())? {
        let metadata_digest = Digest::from_hex(&metadata_hex).ok_or(CacheError::FileNotFound)?;
        if state.blobs.head(&metadata_digest).await?.is_some() {
            return Ok(Bytes::from(
                state
                    .blobs
                    .read_bytes(&metadata_digest, crate::archive::MAX_WHEEL_METADATA_BYTES)
                    .await?,
            ));
        }
        if url != GENERATED_METADATA_URL {
            let upstream = state
                .meta
                .get_file_url(artifact_digest.as_str())?
                .and_then(|source| source.upstream);
            let bytes = match fetch_from_source(state, &source, upstream.as_deref(), &url).await {
                Ok(bytes) => bytes,
                Err(CacheError::Upstream(err)) if err.status() == Some(404) => {
                    state.remember_negative(negative_key, NEGATIVE_TTL_SECS);
                    return Err(CacheError::FileNotFound);
                }
                Err(err) => return Err(err),
            };
            state.blobs.put_bytes_as(&bytes, &metadata_digest).await?;
            return Ok(bytes);
        }
    }
    write_generated_metadata(state, artifact_digest, route, artifact_filename).await
}

async fn write_generated_metadata(
    state: &Arc<ServingState>,
    artifact_digest: &Digest,
    route: &str,
    artifact_filename: &str,
) -> Result<Bytes, CacheError> {
    let (bytes, source) = generated_metadata_bytes(state, artifact_digest, route, artifact_filename).await?;
    let metadata_digest = state.blobs.put_bytes(&bytes).await?;
    let source = source.unwrap_or_else(|| GENERATED_METADATA_URL.to_owned());
    let artifact_sha256 = artifact_digest.as_str();
    let metadata_sha256 = metadata_digest.as_str();
    state
        .meta
        .put_metadata(artifact_sha256, GENERATED_METADATA_URL, metadata_sha256, &source)?;
    super::invalidate_project_route(state, route, &crate::project_of_filename(artifact_filename));
    Ok(Bytes::from(bytes))
}

const GENERATED_METADATA_URL: &str = "peryx:generated";

async fn generated_metadata_bytes(
    state: &Arc<ServingState>,
    artifact_digest: &Digest,
    route: &str,
    filename: &str,
) -> Result<(Vec<u8>, Option<String>), CacheError> {
    let source = state.meta.get_file_url(artifact_digest.as_str())?;
    if state.blobs.head(artifact_digest).await?.is_some() {
        let lease = state.blobs.materialize(artifact_digest).await?;
        let metadata = metadata_from_artifact_path(filename, lease.path())?.ok_or(CacheError::FileNotFound)?;
        return Ok((metadata, source.map(|source| source.source)));
    }
    let Some(source) = source else {
        return Err(CacheError::FileNotFound);
    };
    if let Some(metadata) =
        generated_wheel_metadata_by_range(state, &source.source, source.upstream.as_deref(), &source.url, filename)
            .await?
    {
        return Ok((metadata, Some(source.source)));
    }
    let path = file_path(
        state.clone(),
        artifact_digest.clone(),
        route.to_owned(),
        filename.to_owned(),
    )
    .await?;
    let metadata = metadata_from_artifact_path(filename, path.path())?.ok_or(CacheError::FileNotFound)?;
    Ok((metadata, Some(source.source)))
}

fn metadata_from_artifact_path(filename: &str, path: &std::path::Path) -> Result<Option<Vec<u8>>, CacheError> {
    if is_wheel(filename) {
        return Ok(crate::archive::wheel_metadata_path(filename, path)?);
    }
    if is_tar_gz(filename) {
        return Ok(crate::archive::sdist_metadata_path(filename, path)?);
    }
    Ok(None)
}

async fn generated_wheel_metadata_by_range(
    state: &Arc<ServingState>,
    source_name: &str,
    upstream: Option<&str>,
    url: &str,
    filename: &str,
) -> Result<Option<Vec<u8>>, CacheError> {
    if !is_wheel(filename) {
        return Ok(None);
    }
    let (client, offline) = source_artifact_client(state, source_name, upstream)?;
    if offline {
        return Err(CacheError::OfflineMissing("metadata"));
    }
    let _permit = upstream_permit(state, source_name).await?;
    match wheel_metadata_by_range(&client, url, filename).await {
        Ok(RemoteMetadata::Found(metadata)) => Ok(Some(metadata)),
        Ok(RemoteMetadata::Missing) => Err(CacheError::FileNotFound),
        Ok(RemoteMetadata::Unsupported) | Err(RangeError::Unsupported | RangeError::Invalid(_)) => Ok(None),
        Err(RangeError::Upstream(err)) => Err(CacheError::Upstream(err)),
    }
}

enum RemoteMetadata {
    Found(Vec<u8>),
    Missing,
    Unsupported,
}

async fn wheel_metadata_by_range(
    client: &ArtifactClient,
    url: &str,
    filename: &str,
) -> Result<RemoteMetadata, RangeError> {
    let metadata_path = match crate::archive::wheel_metadata_member_path(filename) {
        Ok(Some(metadata_path)) => metadata_path,
        Ok(None) => return Ok(RemoteMetadata::Unsupported),
        Err(err) => return Err(RangeError::Invalid(err.to_string())),
    };
    let head = client.head_file_for_range(url).await?;
    if head.len == 0 {
        return Ok(RemoteMetadata::Unsupported);
    }
    let tail_start = head.len.saturating_sub(ZIP_TAIL_BYTES);
    let tail = client
        .fetch_range(url, tail_start, head.len - 1, usize::try_from(ZIP_TAIL_BYTES).unwrap())
        .await?;
    let Some(directory) = central_directory(&tail) else {
        return Ok(RemoteMetadata::Unsupported);
    };
    if directory.len == 0 {
        return Ok(RemoteMetadata::Unsupported);
    }
    let directory_end = directory.offset + directory.len - 1;
    let directory_bytes = client
        .fetch_range(
            url,
            directory.offset,
            directory_end,
            usize::try_from(directory.len).unwrap_or(usize::MAX),
        )
        .await?;
    let entry = match find_central_directory_entry(&directory_bytes, &metadata_path) {
        DirectoryEntrySearch::Found(entry) => entry,
        DirectoryEntrySearch::Missing => return Ok(RemoteMetadata::Missing),
        DirectoryEntrySearch::Unsupported | DirectoryEntrySearch::Invalid => return Ok(RemoteMetadata::Unsupported),
    };
    if entry.uncompressed_size > crate::archive::MAX_WHEEL_METADATA_BYTES
        || entry.compressed_size > crate::archive::MAX_WHEEL_METADATA_BYTES
    {
        return Ok(RemoteMetadata::Unsupported);
    }
    let data_start = zip_data_start(client, url, entry.local_header_offset).await?;
    let compressed = if entry.compressed_size == 0 {
        Bytes::new()
    } else {
        client
            .fetch_range(
                url,
                data_start,
                data_start + entry.compressed_size - 1,
                usize::try_from(entry.compressed_size).unwrap_or(usize::MAX),
            )
            .await?
    };
    match entry.compression_method {
        ZIP_COMPRESSION_STORED => Ok(RemoteMetadata::Found(compressed.to_vec())),
        ZIP_COMPRESSION_DEFLATED => {
            let mut decoder = flate2::read::DeflateDecoder::new(Cursor::new(compressed));
            let mut metadata = Vec::with_capacity(usize::try_from(entry.uncompressed_size).unwrap());
            if let Err(err) = decoder.read_to_end(&mut metadata) {
                return Err(RangeError::Invalid(err.to_string()));
            }
            if metadata.len() as u64 == entry.uncompressed_size {
                Ok(RemoteMetadata::Found(metadata))
            } else {
                Ok(RemoteMetadata::Unsupported)
            }
        }
        _ => Ok(RemoteMetadata::Unsupported),
    }
}

async fn zip_data_start(client: &ArtifactClient, url: &str, local_header_offset: u64) -> Result<u64, RangeError> {
    let header = client
        .fetch_range(url, local_header_offset, local_header_offset + 29, 30)
        .await?;
    if !header.starts_with(&ZIP_LOCAL_SIGNATURE) {
        return Err(RangeError::Invalid("hosted file header signature mismatch".to_owned()));
    }
    let name_len = u64::from(read_u16(&header, 26).expect("fixed hosted header range is complete"));
    let extra_len = u64::from(read_u16(&header, 28).expect("fixed hosted header range is complete"));
    Ok(local_header_offset + 30 + name_len + extra_len)
}

/// Queue wheel metadata generation without delaying the page response.
///
/// Admission precedes spawning so saturated traffic cannot create waiting tasks. Sdists remain
/// on-demand because extracting their metadata requires a full download.
pub(super) fn spawn_metadata_backfill(state: &Arc<ServingState>, route: String, registrations: &[Registration]) {
    let candidates = metadata_backfill_candidates(registrations);
    if candidates.is_empty() {
        return;
    }
    state
        .plugin_service::<MetadataBackfills>()
        .expect("PyPI runtime installs metadata backfills")
        .spawn(state.clone(), route, candidates);
}

const BACKFILL_CONCURRENCY: usize = 2;

pub(super) struct MetadataBackfills {
    slots: Arc<tokio::sync::Semaphore>,
    tasks: Mutex<tokio::task::JoinSet<()>>,
}

impl MetadataBackfills {
    fn spawn(&self, state: Arc<ServingState>, route: String, candidates: Vec<MetadataBackfillCandidate>) {
        let Ok(slot) = self.slots.clone().try_acquire_owned() else {
            return;
        };
        let mut tasks = self.tasks.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        while let Some(result) = tasks.try_join_next() {
            if let Err(err) = result {
                tracing::error!(?err, "metadata backfill task failed");
            }
        }
        tasks.spawn(async move {
            run_metadata_backfill_candidates(state, route, candidates).await;
            drop(slot);
        });
    }
}

impl Default for MetadataBackfills {
    fn default() -> Self {
        Self {
            slots: Arc::new(tokio::sync::Semaphore::new(BACKFILL_CONCURRENCY)),
            tasks: Mutex::new(tokio::task::JoinSet::new()),
        }
    }
}

impl Drop for MetadataBackfills {
    fn drop(&mut self) {
        self.tasks
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .abort_all();
    }
}

fn metadata_backfill_candidates(registrations: &[Registration]) -> Vec<MetadataBackfillCandidate> {
    registrations
        .iter()
        .filter(|registration| registration.metadata.is_none() && is_wheel(&registration.filename))
        .filter_map(|registration| {
            Some(MetadataBackfillCandidate {
                digest: Digest::from_hex(&registration.sha256)?,
                filename: registration.filename.clone(),
            })
        })
        .collect()
}

async fn run_metadata_backfill_candidates(
    state: Arc<ServingState>,
    route: String,
    candidates: Vec<MetadataBackfillCandidate>,
) {
    for candidate in candidates {
        if state
            .meta
            .get_metadata(candidate.digest.as_str())
            .is_ok_and(|record| record.is_some())
        {
            continue;
        }
        let Err(err) = write_generated_metadata(&state, &candidate.digest, &route, &candidate.filename).await else {
            continue;
        };
        let digest = candidate.digest.as_str();
        let filename = &candidate.filename;
        tracing::debug!(?err, digest, filename = %filename, "metadata backfill skipped");
    }
}

struct MetadataBackfillCandidate {
    digest: Digest,
    filename: String,
}

/// # Errors
/// Returns [`CacheError`] when the metadata store cannot be read.
pub fn registered_file_size(state: &ServingState, digest: &Digest) -> Result<Option<u64>, CacheError> {
    Ok(state.meta.get_file_url(digest.as_str())?.and_then(|source| source.size))
}

fn metadata_negative_key(artifact_digest: &Digest) -> String {
    format!("metadata\0{}", artifact_digest.as_str())
}

#[cfg(test)]
#[path = "../../tests/unit/cache/metadata/tests.rs"]
mod tests;
