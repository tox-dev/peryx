//! The read-through cache: serve a project's simple page and file bytes, fetching and caching from
//! upstream on a miss.

use bytes::Bytes;
use velox_core::pypi::{File, Meta, ProjectDetail, parse_detail, to_json};
use velox_storage::blob::Digest;
use velox_storage::meta::CachedIndex;

use crate::state::AppState;

/// An error while producing a cached response.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error(transparent)]
    Meta(#[from] velox_storage::meta::MetaError),
    #[error(transparent)]
    Blob(#[from] velox_storage::blob::BlobError),
    #[error(transparent)]
    Upstream(#[from] velox_upstream::UpstreamError),
    #[error(transparent)]
    Parse(#[from] serde_json::Error),
    #[error("upstream unreachable and nothing cached")]
    Unavailable,
    #[error("no known source for this file")]
    FileNotFound,
}

/// Resolve a project's detail, serving from cache when fresh and revalidating or fetching upstream
/// otherwise. Returns `None` when the project does not exist upstream.
///
/// # Errors
/// Returns [`CacheError`] if a store, parse, or (with no cached fallback) upstream error occurs.
pub async fn project_detail(state: &AppState, project: &str) -> Result<Option<ProjectDetail>, CacheError> {
    let key = format!("{}/{project}", state.index);
    let now = (state.clock)();
    let cached = state.meta.get_index(&key)?;

    if let Some(record) = &cached
        && now - record.fetched_at_unix < state.ttl_secs
    {
        return Ok(Some(decode_detail(&record.body)?));
    }

    let etag = cached.as_ref().and_then(|record| record.etag.clone());
    match state.upstream.fetch_project(project, etag.as_deref()).await {
        Ok(response) if response.status == 200 => Ok(Some(store_fresh(
            state,
            &key,
            &response.body,
            response.etag,
            response.last_serial,
            now,
        )?)),
        Ok(response) if response.status == 304 => {
            let mut record = cached.ok_or(CacheError::Unavailable)?;
            record.fetched_at_unix = now;
            state.meta.put_index(&key, &record)?;
            Ok(Some(decode_detail(&record.body)?))
        }
        Ok(response) if response.status == 404 => Ok(None),
        Ok(_) => serve_stale(cached.as_ref()),
        Err(err) => match cached.as_ref() {
            Some(record) => Ok(Some(decode_detail(&record.body)?)),
            None => Err(CacheError::Upstream(err)),
        },
    }
}

fn serve_stale(cached: Option<&CachedIndex>) -> Result<Option<ProjectDetail>, CacheError> {
    match cached {
        Some(record) => Ok(Some(decode_detail(&record.body)?)),
        None => Err(CacheError::Unavailable),
    }
}

fn store_fresh(
    state: &AppState,
    key: &str,
    body: &[u8],
    etag: Option<String>,
    last_serial: Option<u64>,
    now: i64,
) -> Result<ProjectDetail, CacheError> {
    let parsed = parse_detail(body)?;
    let files = parsed
        .files
        .into_iter()
        .map(|file| register_file(state, file))
        .collect::<Result<Vec<_>, _>>()?;
    let detail = ProjectDetail {
        meta: Meta::default(),
        name: parsed.name,
        versions: parsed.versions,
        files,
    };
    let record = CachedIndex {
        etag,
        last_serial,
        fetched_at_unix: now,
        body: to_json(&detail).into_bytes(),
    };
    state.meta.put_index(key, &record)?;
    Ok(detail)
}

/// Record a file's upstream URL under its digest and rewrite its URL to velox's own file route.
/// A file without a sha256 hash is left as-is (it cannot be content-addressed).
fn register_file(state: &AppState, mut file: File) -> Result<File, CacheError> {
    let Some(sha256) = file.hashes.get("sha256").cloned() else {
        return Ok(file);
    };
    state.meta.put_file_url(&sha256, &file.url)?;
    file.url = format!("/{}/files/{sha256}/{}", state.index, file.filename);
    Ok(file)
}

fn decode_detail(body: &[u8]) -> Result<ProjectDetail, CacheError> {
    let parsed = parse_detail(body)?;
    Ok(ProjectDetail {
        meta: Meta::default(),
        name: parsed.name,
        versions: parsed.versions,
        files: parsed.files,
    })
}

/// Resolve a file's bytes, serving the cached blob or fetching, verifying, and caching it.
///
/// # Errors
/// Returns [`CacheError::FileNotFound`] if the digest has no known source, or another
/// [`CacheError`] on a store or upstream failure.
pub async fn file_bytes(state: &AppState, digest: &Digest) -> Result<Bytes, CacheError> {
    if state.blobs.exists(digest) {
        return Ok(Bytes::from(state.blobs.read(digest)?));
    }
    let url = state
        .meta
        .get_file_url(digest.as_str())?
        .ok_or(CacheError::FileNotFound)?;
    let bytes = state.upstream.fetch_bytes(&url).await?;
    state.blobs.write_verified(&bytes, digest)?;
    Ok(bytes)
}
