//! PEP 658 metadata resolution: cached sidecars, ranged wheel reads, and background backfill.

use std::sync::{Arc, Mutex};

use crate::store::PypiStore as _;
use crate::store::{FilePublication, MetadataClaim};
use crate::stream::Registration;
use bytes::Bytes;
use peryx_driver::state::ServingState;
use peryx_ha::ArtifactSource;
use peryx_index::{Index, IndexKind};
use peryx_storage::blob::Digest;
use peryx_upstream::{ArtifactClient, RangeError, RangeSession};

use crate::archive::{
    MAX_ZIP_CENTRAL_DIRECTORY_BYTES, ZIP_TAIL_BYTES, ZipEntry, ZipEntrySearch, find_zip_entry, zip_central_directory,
};

use super::download::file_path;
use super::{
    CacheError, NEGATIVE_TTL_SECS, ensure_digest_clear, is_tar_gz, is_wheel, source_artifact_client, source_client,
    upstream_permit,
};

const _: () = assert!(crate::archive::MAX_WHEEL_METADATA_BYTES < usize::MAX as u64);
const _: () = assert!(MAX_ZIP_CENTRAL_DIRECTORY_BYTES <= usize::MAX as u64);

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

/// Resolve an artifact's PEP 658 metadata bytes for the publication `index` serves it as.
///
/// The winning publication decides first. Only the index that listed this file may lend it a PEP
/// 658 sidecar, because a claim is the publisher's word about its own URL rather than a property
/// of the artifact bytes. Metadata peryx derived from those bytes is shared by digest, since every
/// publication of a digest has the same METADATA to extract.
///
/// # Errors
/// Returns [`CacheError::FileNotFound`] if the artifact has no usable metadata source, or another
/// error on a store, archive, or upstream failure.
pub async fn metadata_bytes(
    state: &Arc<ServingState>,
    index: &Index,
    artifact_digest: &Digest,
    route: &str,
    metadata_filename: &str,
) -> Result<Bytes, CacheError> {
    ensure_digest_clear(state, artifact_digest)?;
    let artifact_filename = metadata_filename
        .strip_suffix(".metadata")
        .ok_or(CacheError::FileNotFound)?;
    let publication = winning_publication(
        state,
        index,
        &crate::project_of_filename(artifact_filename),
        artifact_digest.as_str(),
        artifact_filename,
    )?;
    if let Some(FilePublication::Claimed(claim)) = publication {
        return claimed_metadata(state, &claim, artifact_digest, route, artifact_filename).await;
    }
    if let Some(metadata_hex) = state.meta.get_metadata_digest(artifact_digest.as_str())? {
        let metadata_digest = Digest::from_hex(&metadata_hex).ok_or(CacheError::FileNotFound)?;
        if state.blobs.head(&metadata_digest).await?.is_some() {
            return read_metadata_blob(state, &metadata_digest).await;
        }
    }
    write_generated_metadata(state, artifact_digest, route, artifact_filename).await
}

/// Whether `index` publishes `filename` at `digest`.
///
/// The blob store and the digest-to-source locator are both process-wide, so any route that takes a
/// digest from its URL can reach bytes another index cached unless it first proves the pair belongs
/// to the index it names. This is that proof, and it is the only membership rule peryx applies:
/// every route that releases artifact bytes reaches it through the download gate. See #1308.
///
/// A sidecar is published by its artifact - both carry the artifact's digest in the URL - so the
/// pair is judged on the artifact's own publication.
///
/// # Errors
/// Returns [`CacheError`] when the store cannot be read.
pub fn publishes_file(
    state: &ServingState,
    index: &Index,
    filename: &str,
    digest: &Digest,
) -> Result<bool, CacheError> {
    let artifact = super::artifact_of(filename);
    Ok(winning_publication(
        state,
        index,
        &crate::project_of_filename(artifact),
        digest.as_str(),
        artifact,
    )?
    .is_some())
}

/// The publication of `filename`/`sha256` that `index` serves, or `None` when it serves none.
///
/// A virtual index answers with the first leaf in shadow order that published the file, the leaf
/// whose page entry won the merge, so a shadowed leaf's sidecar stays where it is. Nesting is
/// flattened rather than descended per layer, because a container carries neither publications nor a
/// source class of its own.
fn winning_publication(
    state: &ServingState,
    index: &Index,
    project: &str,
    sha256: &str,
    filename: &str,
) -> Result<Option<FilePublication>, CacheError> {
    match &index.kind {
        IndexKind::Cached { .. } => Ok(state
            .meta
            .get_file_publication(&index.name, project, sha256, filename)?),
        // A hosted file's metadata comes out of the bytes peryx holds, so it carries no claim. It
        // still owns the publication, which stops the walk before a proxied layer's claim.
        IndexKind::Hosted { .. } => Ok(
            hosted_publication(state, &index.name, project, sha256, filename)?.then_some(FilePublication::Unclaimed)
        ),
        IndexKind::Virtual { layers, .. } => {
            for position in peryx_index::leaf_order(&state.indexes, layers) {
                let leaf = state.index_at(position);
                if let Some(publication) = winning_publication(state, leaf, project, sha256, filename)? {
                    return Ok(Some(publication));
                }
            }
            Ok(None)
        }
    }
}

fn hosted_publication(
    state: &ServingState,
    index: &str,
    project: &str,
    sha256: &str,
    filename: &str,
) -> Result<bool, CacheError> {
    let Some(record) = state.meta.get_upload(index, project, filename)? else {
        return Ok(false);
    };
    let uploaded: crate::upload::Uploaded = serde_json::from_slice(&record)?;
    Ok(uploaded.trashed.is_none() && uploaded.file.sha256() == Some(sha256))
}

/// Serve the sidecar a publication advertised, fetching it through that publication's own source on
/// a blob miss. The blob store commits it under the advertised digest, so bytes that do not hash to
/// what the page promised never reach a reader.
async fn claimed_metadata(
    state: &Arc<ServingState>,
    claim: &MetadataClaim,
    artifact_digest: &Digest,
    route: &str,
    artifact_filename: &str,
) -> Result<Bytes, CacheError> {
    let metadata_digest = Digest::from_hex(&claim.metadata_sha256).ok_or(CacheError::FileNotFound)?;
    let negative_key = metadata_negative_key(claim);
    if state.negative_fresh(&negative_key) {
        return Err(CacheError::FileNotFound);
    }
    if state.blobs.head(&metadata_digest).await?.is_some() {
        return read_metadata_blob(state, &metadata_digest).await;
    }
    let bytes = match fetch_from_source(state, &claim.source, claim.upstream.as_deref(), &claim.url).await {
        Ok(bytes) => bytes,
        // Only absence is recoverable. An auth failure, a rate limit, a timeout or a server error
        // all say the sidecar may still be there, so they keep their own status and retry semantics.
        Err(CacheError::Upstream(err)) if err.status() == Some(404) => {
            return recover_claimed_metadata(
                state,
                &metadata_digest,
                artifact_digest,
                route,
                artifact_filename,
                negative_key,
            )
            .await;
        }
        Err(err) => return Err(err),
    };
    state.blobs.put_bytes_as(&bytes, &metadata_digest).await?;
    record_sidecar_placement(state, &metadata_digest, ArtifactSource::Proxy);
    Ok(bytes)
}

/// Rebuild a sidecar the publishing index advertised but no longer serves, out of the artifact itself.
///
/// Under PEP 658 the advertisement is peryx's own promise to the installer, so a vanished sibling is
/// peryx's inconsistency to repair rather than upstream's 404 to forward. The rebuilt bytes hold only
/// when they hash to the digest the page published, because an installer verifies that digest and
/// anything else trades a 404 for a checksum failure.
///
/// Extraction that produces nothing leaves the sidecar as unavailable as the 404 found it, so the
/// negative entry still goes in. It says peryx could not produce the sidecar inside its 30-second
/// life, not that the artifact lacks metadata.
async fn recover_claimed_metadata(
    state: &Arc<ServingState>,
    metadata_digest: &Digest,
    artifact_digest: &Digest,
    route: &str,
    artifact_filename: &str,
    negative_key: String,
) -> Result<Bytes, CacheError> {
    let bytes = match generated_metadata_bytes(state, artifact_digest, route, artifact_filename).await {
        Ok(bytes) => bytes,
        Err(err) => {
            let digest = artifact_digest.as_str();
            tracing::debug!(
                ?err,
                digest,
                artifact_filename,
                "advertised metadata recovery found nothing"
            );
            state.remember_negative(negative_key, NEGATIVE_TTL_SECS);
            return Err(CacheError::FileNotFound);
        }
    };
    if Digest::of(&bytes) != *metadata_digest {
        return Err(CacheError::AdvertisedMetadataMismatch);
    }
    state.blobs.put_bytes_as(&bytes, metadata_digest).await?;
    record_generated_metadata(state, artifact_digest, metadata_digest, route, artifact_filename)?;
    Ok(Bytes::from(bytes))
}

async fn read_metadata_blob(state: &Arc<ServingState>, metadata_digest: &Digest) -> Result<Bytes, CacheError> {
    Ok(Bytes::from(
        state
            .blobs
            .read_bytes(metadata_digest, crate::archive::MAX_WHEEL_METADATA_BYTES)
            .await?,
    ))
}

async fn write_generated_metadata(
    state: &Arc<ServingState>,
    artifact_digest: &Digest,
    route: &str,
    artifact_filename: &str,
) -> Result<Bytes, CacheError> {
    let bytes = generated_metadata_bytes(state, artifact_digest, route, artifact_filename).await?;
    let metadata_digest = state.blobs.put_bytes(&bytes).await?;
    record_generated_metadata(state, artifact_digest, &metadata_digest, route, artifact_filename)?;
    Ok(Bytes::from(bytes))
}

fn record_generated_metadata(
    state: &ServingState,
    artifact_digest: &Digest,
    metadata_digest: &Digest,
    route: &str,
    artifact_filename: &str,
) -> Result<(), CacheError> {
    state
        .meta
        .put_metadata(artifact_digest.as_str(), metadata_digest.as_str())?;
    record_sidecar_placement(state, metadata_digest, ArtifactSource::Generated);
    super::invalidate_project_route(state, route, &crate::project_of_filename(artifact_filename));
    Ok(())
}

/// Record the sidecar blob this node just committed, so the projection answers for it the way it does
/// for the artifact beside it.
///
/// A sidecar is an artifact by the same definition as the wheel it describes: immutable bytes
/// addressed by digest. A store fault leaves the bytes committed and the projection behind them rather
/// than failing a read whose bytes are already on disk.
fn record_sidecar_placement(state: &ServingState, metadata_digest: &Digest, source: ArtifactSource) {
    state.meta.record_committed_placement(metadata_digest.as_str(), source);
}

async fn generated_metadata_bytes(
    state: &Arc<ServingState>,
    artifact_digest: &Digest,
    route: &str,
    filename: &str,
) -> Result<Vec<u8>, CacheError> {
    let source = state.meta.get_file_url(artifact_digest.as_str())?;
    if state.blobs.head(artifact_digest).await?.is_some() {
        let lease = state.blobs.materialize(artifact_digest).await?;
        return metadata_from_artifact_path(filename, lease.path())?.ok_or(CacheError::FileNotFound);
    }
    let Some(source) = source else {
        return Err(CacheError::FileNotFound);
    };
    if let Some(metadata) =
        generated_wheel_metadata_by_range(state, &source.source, source.upstream.as_deref(), &source.url, filename)
            .await?
    {
        return Ok(metadata);
    }
    let path = file_path(
        state.clone(),
        artifact_digest.clone(),
        route.to_owned(),
        filename.to_owned(),
    )
    .await?;
    metadata_from_artifact_path(filename, path.path())?.ok_or(CacheError::FileNotFound)
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
    let session = client.range_session(url).await?;
    if session.is_empty() {
        return Ok(RemoteMetadata::Unsupported);
    }
    let tail_start = session.len().saturating_sub(ZIP_TAIL_BYTES);
    let tail = session
        .fetch_range(tail_start, session.len() - 1, usize::try_from(ZIP_TAIL_BYTES).unwrap())
        .await?;
    let Some(directory) = zip_central_directory(&tail, session.len()) else {
        return Ok(RemoteMetadata::Unsupported);
    };
    if directory.len == 0 {
        return Ok(RemoteMetadata::Unsupported);
    }
    let directory_bytes = session
        .fetch_range(
            directory.offset,
            directory.offset + directory.len - 1,
            usize::try_from(directory.len).expect("the central-directory budget fits in memory"),
        )
        .await?;
    let entry = match find_zip_entry(&directory_bytes, &metadata_path) {
        ZipEntrySearch::Found(entry) => entry,
        ZipEntrySearch::Missing => return Ok(RemoteMetadata::Missing),
        ZipEntrySearch::Unsupported | ZipEntrySearch::Invalid => return Ok(RemoteMetadata::Unsupported),
    };
    if entry.uncompressed_size > crate::archive::MAX_WHEEL_METADATA_BYTES
        || entry.compressed_size > crate::archive::MAX_WHEEL_METADATA_BYTES
    {
        return Ok(RemoteMetadata::Unsupported);
    }
    let data_start = zip_data_start(&session, &entry).await?;
    let compressed = if entry.compressed_size == 0 {
        Bytes::new()
    } else {
        session
            .fetch_range(
                data_start,
                data_start + entry.compressed_size - 1,
                usize::try_from(entry.compressed_size).expect("the metadata budget fits in memory"),
            )
            .await?
    };
    // A wheel whose own ZIP records contradict each other, or whose member does not hash to the
    // CRC-32 it declares, says nothing trustworthy about its metadata: give up the ranged read for
    // the digest-verified full download rather than publish what the range returned.
    entry
        .decode(&compressed)
        .map(RemoteMetadata::Found)
        .map_err(|err| RangeError::Invalid(err.to_string()))
}

async fn zip_data_start(session: &RangeSession, entry: &ZipEntry) -> Result<u64, RangeError> {
    let len = entry.local_header_len();
    let header = session
        .fetch_range(
            entry.local_header_offset,
            entry.local_header_offset + len - 1,
            usize::try_from(len).expect("a local header fits in memory"),
        )
        .await?;
    entry
        .data_start(&header)
        .map_err(|err| RangeError::Invalid(err.to_string()))
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
            .get_metadata_digest(candidate.digest.as_str())
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

/// Key the negative entry on the sidecar that went missing rather than on the artifact, so one
/// index's dead claim leaves metadata another index derives from the same bytes reachable.
fn metadata_negative_key(claim: &MetadataClaim) -> String {
    format!("metadata\0{}\0{}", claim.source, claim.url)
}

#[cfg(test)]
#[path = "../../tests/unit/cache/metadata/tests.rs"]
mod tests;
