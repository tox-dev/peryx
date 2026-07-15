//! PEP 658 metadata resolution: cached sidecars, ranged wheel reads, and background backfill.

use std::io::{Cursor, Read as _};
use std::sync::Arc;

use crate::store::PypiStore as _;
use crate::stream::Registration;
use bytes::Bytes;
use peryx_driver::state::ServingState;
use peryx_storage::blob::Digest;
use peryx_upstream::{RangeError, UpstreamClient};

mod central_dir;
use central_dir::{
    DirectoryEntrySearch, ZIP_COMPRESSION_DEFLATED, ZIP_COMPRESSION_STORED, ZIP_LOCAL_SIGNATURE, ZIP_TAIL_BYTES,
    central_directory, find_central_directory_entry, read_u16,
};

use super::download::file_path;
use super::{CacheError, NEGATIVE_TTL_SECS, is_tar_gz, is_wheel, source_mirror, upstream_permit};

/// Fetch a URL through the named cached's client (reusing its authentication).
async fn fetch_from_source(state: &ServingState, source: &str, url: &str) -> Result<Bytes, CacheError> {
    let (client, offline) = source_mirror(state, source)?;
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
    let artifact_filename = metadata_filename
        .strip_suffix(".metadata")
        .ok_or(CacheError::FileNotFound)?;
    let negative_key = metadata_negative_key(artifact_digest);
    if state.negative_fresh(&negative_key) {
        return Err(CacheError::FileNotFound);
    }
    if let Some((url, metadata_hex, source)) = state.meta.get_metadata(artifact_digest.as_str())? {
        let metadata_digest = Digest::from_hex(&metadata_hex).ok_or(CacheError::FileNotFound)?;
        if state.blobs.exists(&metadata_digest) {
            return Ok(Bytes::from(state.blobs.read(&metadata_digest)?));
        }
        if url != GENERATED_METADATA_URL {
            let bytes = match fetch_from_source(state, &source, &url).await {
                Ok(bytes) => bytes,
                Err(CacheError::Upstream(err)) if err.status() == Some(404) => {
                    state.remember_negative(negative_key, NEGATIVE_TTL_SECS);
                    return Err(CacheError::FileNotFound);
                }
                Err(err) => return Err(err),
            };
            state.blobs.write_verified(&bytes, &metadata_digest)?;
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
    let metadata_digest = state.blobs.write(&bytes)?;
    let source = source.unwrap_or_else(|| GENERATED_METADATA_URL.to_owned());
    let artifact_sha256 = artifact_digest.as_str();
    let metadata_sha256 = metadata_digest.as_str();
    state
        .meta
        .put_metadata(artifact_sha256, GENERATED_METADATA_URL, metadata_sha256, &source)?;
    state.invalidate_project(&crate::project_of_filename(artifact_filename));
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
    if state.blobs.exists(artifact_digest) {
        let metadata = metadata_from_artifact_path(filename, &state.blobs.path_for(artifact_digest))?
            .ok_or(CacheError::FileNotFound)?;
        return Ok((metadata, source.map(|source| source.source)));
    }
    let Some(source) = source else {
        return Err(CacheError::FileNotFound);
    };
    if let Some(metadata) = generated_wheel_metadata_by_range(state, &source.source, &source.url, filename).await? {
        return Ok((metadata, Some(source.source)));
    }
    let path = file_path(
        state.clone(),
        artifact_digest.clone(),
        route.to_owned(),
        filename.to_owned(),
    )
    .await?;
    let metadata = metadata_from_artifact_path(filename, &path)?.ok_or(CacheError::FileNotFound)?;
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
    url: &str,
    filename: &str,
) -> Result<Option<Vec<u8>>, CacheError> {
    if !is_wheel(filename) {
        return Ok(None);
    }
    let (client, offline) = source_mirror(state, source_name)?;
    if offline {
        return Err(CacheError::OfflineMissing("metadata"));
    }
    if !client.may_support_ranges() {
        return Ok(None);
    }
    let _permit = upstream_permit(state, source_name).await?;
    match wheel_metadata_by_range(&client, url, filename).await {
        Ok(RemoteMetadata::Found(metadata)) => Ok(Some(metadata)),
        Ok(RemoteMetadata::Missing) => Err(CacheError::FileNotFound),
        Ok(RemoteMetadata::Unsupported) => Ok(None),
        Err(RangeError::Upstream(err)) => Err(CacheError::Upstream(err)),
        Err(err @ (RangeError::Unsupported | RangeError::Invalid(_))) => {
            debug_assert!(err.disables_ranges());
            client.disable_ranges();
            Ok(None)
        }
    }
}

enum RemoteMetadata {
    Found(Vec<u8>),
    Missing,
    Unsupported,
}

async fn wheel_metadata_by_range(
    client: &UpstreamClient,
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
    let tail = client.fetch_range(url, tail_start, head.len - 1).await?;
    let Some(directory) = central_directory(&tail) else {
        return Ok(RemoteMetadata::Unsupported);
    };
    if directory.len == 0 {
        return Ok(RemoteMetadata::Unsupported);
    }
    let directory_end = directory.offset + directory.len - 1;
    let directory_bytes = client.fetch_range(url, directory.offset, directory_end).await?;
    let entry = match find_central_directory_entry(&directory_bytes, &metadata_path) {
        DirectoryEntrySearch::Found(entry) => entry,
        DirectoryEntrySearch::Missing => return Ok(RemoteMetadata::Missing),
        DirectoryEntrySearch::Invalid => return Ok(RemoteMetadata::Unsupported),
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
            .fetch_range(url, data_start, data_start + entry.compressed_size - 1)
            .await?
    };
    match entry.compression_method {
        ZIP_COMPRESSION_STORED => Ok(RemoteMetadata::Found(compressed.to_vec())),
        ZIP_COMPRESSION_DEFLATED => {
            let mut decoder = flate2::read::DeflateDecoder::new(Cursor::new(compressed));
            let mut metadata = Vec::with_capacity(usize::try_from(entry.uncompressed_size).unwrap_or_default());
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

async fn zip_data_start(client: &UpstreamClient, url: &str, local_header_offset: u64) -> Result<u64, RangeError> {
    let header = client
        .fetch_range(url, local_header_offset, local_header_offset + 29)
        .await?;
    if !header.starts_with(&ZIP_LOCAL_SIGNATURE) {
        return Err(RangeError::Invalid("hosted file header signature mismatch".to_owned()));
    }
    let name_len = u64::from(read_u16(&header, 26).expect("fixed hosted header range is complete"));
    let extra_len = u64::from(read_u16(&header, 28).expect("fixed hosted header range is complete"));
    Ok(local_header_offset + 30 + name_len + extra_len)
}

/// Pre-warm PEP 658 metadata after a page is served so a later visit advertises it, without blocking
/// the page response or the downloads an in-flight install is waiting on. Two guards keep the detached
/// task from competing with live traffic: only wheels are eligible (their metadata is a cheap ranged
/// read of the archive's `METADATA` member, whereas an sdist needs a full download plus a gunzip, so
/// sdist metadata is generated only when a client actually requests `<sdist>.metadata`), and
/// generation runs under [`BACKFILL_CONCURRENCY`]. On-demand `.metadata` requests bypass both.
pub(super) fn spawn_metadata_backfill(state: Arc<ServingState>, route: String, registrations: &[Registration]) {
    let candidates = metadata_backfill_candidates(registrations);
    if candidates.is_empty() {
        return;
    }
    tokio::spawn(async move {
        run_metadata_backfill_candidates(state, route, candidates).await;
    });
}

const BACKFILL_CONCURRENCY: usize = 2;

fn backfill_limiter() -> &'static tokio::sync::Semaphore {
    static LIMITER: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    LIMITER.get_or_init(|| tokio::sync::Semaphore::new(BACKFILL_CONCURRENCY))
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
        let _slot = backfill_limiter()
            .acquire()
            .await
            .expect("backfill limiter is never closed");
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

/// The file size registered from the Simple API page for a digest, when upstream advertised one.
///
/// # Errors
/// Returns [`CacheError`] when the metadata store cannot be read.
pub fn registered_file_size(state: &ServingState, digest: &Digest) -> Result<Option<u64>, CacheError> {
    Ok(state.meta.get_file_url(digest.as_str())?.and_then(|source| source.size))
}

fn metadata_negative_key(artifact_digest: &Digest) -> String {
    format!("metadata\0{}", artifact_digest.as_str())
}

#[cfg(test)]
mod tests {
    use peryx_storage::blob::BlobStore;
    use peryx_storage::meta::MetaStore;

    use super::*;

    #[test]
    fn test_metadata_from_artifact_path_skips_unsupported_formats() {
        assert!(
            metadata_from_artifact_path("pkg-1.0.zip", std::path::Path::new("unused"))
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn test_wheel_metadata_by_range_rejects_invalid_names_before_fetch() {
        let client = UpstreamClient::new("https://pypi.org/simple/").unwrap();

        assert!(matches!(
            wheel_metadata_by_range(&client, "https://example.invalid/pkg.zip", "pkg-1.0.zip").await,
            Ok(RemoteMetadata::Unsupported)
        ));
        assert!(matches!(
            wheel_metadata_by_range(&client, "https://example.invalid/pkg.whl", "pkg.whl").await,
            Err(RangeError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn test_metadata_bytes_regenerates_missing_generated_blob() {
        let (_dir, state) = test_state();
        let wheel = test_wheel(b"Metadata-Version: 2.1\nName: pkg\nVersion: 1.0\n");
        let digest = state.blobs.write(&wheel).unwrap();
        state
            .meta
            .put_metadata(
                digest.as_str(),
                GENERATED_METADATA_URL,
                &"f".repeat(64),
                GENERATED_METADATA_URL,
            )
            .unwrap();

        let bytes = metadata_bytes(&state, &digest, "pypi", "pkg-1.0-py3-none-any.whl.metadata")
            .await
            .unwrap();

        assert_eq!(&bytes[..], b"Metadata-Version: 2.1\nName: pkg\nVersion: 1.0\n");
        assert!(state.meta.get_metadata(digest.as_str()).unwrap().is_some());
    }

    #[tokio::test]
    async fn test_metadata_backfill_candidates_skip_existing_and_successful_records() {
        let (_dir, state) = test_state();
        let existing = Digest::of(b"existing");
        state
            .meta
            .put_metadata(
                existing.as_str(),
                GENERATED_METADATA_URL,
                &"e".repeat(64),
                GENERATED_METADATA_URL,
            )
            .unwrap();
        let wheel = test_wheel(b"Metadata-Version: 2.1\nName: pkg\nVersion: 1.0\n");
        let digest = state.blobs.write(&wheel).unwrap();

        run_metadata_backfill_candidates(
            state.clone(),
            "pypi".to_owned(),
            vec![
                MetadataBackfillCandidate {
                    digest: existing,
                    filename: "pkg-1.0-py3-none-any.whl".to_owned(),
                },
                MetadataBackfillCandidate {
                    digest: digest.clone(),
                    filename: "pkg-1.0-py3-none-any.whl".to_owned(),
                },
            ],
        )
        .await;

        assert!(state.meta.get_metadata(digest.as_str()).unwrap().is_some());
    }

    fn test_state() -> (tempfile::TempDir, Arc<ServingState>) {
        let dir = tempfile::tempdir().unwrap();
        let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
        let blobs = BlobStore::new(dir.path().join("blobs"));
        (dir, peryx_driver::AppState::new(meta, blobs, 60, Vec::new()).serving)
    }

    fn test_wheel(metadata: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut bytes));
            let options = zip::write::SimpleFileOptions::default();
            zip.start_file("pkg-1.0.dist-info/METADATA", options).unwrap();
            std::io::Write::write_all(&mut zip, metadata).unwrap();
            zip.finish().unwrap();
        }
        bytes
    }
}
