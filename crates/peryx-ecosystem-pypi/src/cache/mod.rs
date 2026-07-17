//! The read-through cache and index composition: serve a project's simple page and file bytes across
//! an index's layers, fetching and caching from upstream on a miss.
//!
//! The work is split into cohesive submodules; this spine holds the shared error type, the
//! single-flight/permit/freshness primitives every path shares, and the module wiring.

use crate::store::CachedIndex;
use crate::store::PypiStore as _;
use crate::upload;
use peryx_driver::rate_limit::UpstreamPermit;
use peryx_driver::state::ServingState;
use peryx_index::{Index, IndexKind};
use peryx_policy::PolicyDenial;
use peryx_upstream::{ArtifactClient, UpstreamClient};

mod download;
mod fetch;
mod metadata;
mod mutate;
mod page_stream;
mod resolve;

pub use download::{FileOutcome, FileProbe, file_path, probe_file, stream_file};
pub use fetch::{RefreshSummary, refresh_stale_pages};
pub use metadata::{metadata_bytes, registered_file_size};
pub use mutate::{
    TrashContext, download_status, project_status, project_upload_bytes, promote_release, remove_files, restore_files,
    set_yanked, store_upload,
};
pub use page_stream::{PageOutcome, materialize_detail, stream_detail};
pub use resolve::{DetailPage, list_serial, resolve_detail, resolve_detail_page, resolve_list};

#[cfg(test)]
pub(crate) use download::tail_download;
#[cfg(test)]
pub(crate) use fetch::persist_page;

const NEGATIVE_TTL_SECS: i64 = 30;

/// An error while producing a cached response.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error(transparent)]
    Meta(#[from] peryx_storage::meta::MetaError),
    #[error(transparent)]
    Blob(#[from] peryx_storage::blob::BlobError),
    #[error(transparent)]
    Upstream(#[from] peryx_upstream::UpstreamError),
    #[error(transparent)]
    Parse(#[from] serde_json::Error),
    #[error(transparent)]
    Simple(crate::SimpleError),
    #[error(transparent)]
    Archive(#[from] crate::archive::ArchiveError),
    #[error("upstream unreachable and nothing cached")]
    Unavailable,
    #[error("offline mode has no cached {0}")]
    OfflineMissing(&'static str),
    #[error("index is not volatile; delete is disabled")]
    NotVolatile,
    #[error("no known source for this file")]
    FileNotFound,
    #[error("file already exists: {0}")]
    FileExists(String),
    #[error("file record lacks sha256: {0}")]
    MissingSha256(String),
    #[error("no uploaded files matched source {source_index:?}, project {project:?}, version {version:?}")]
    NoPromotableFiles {
        source_index: String,
        project: String,
        version: String,
    },
    #[error("file stream failed: {0}")]
    Stream(String),
    #[error("rate limit exceeded; retry after {retry_after} seconds")]
    RateLimited { retry_after: u64 },
    #[error(transparent)]
    Policy(#[from] PolicyDenial),
}

impl From<crate::SimpleError> for CacheError {
    fn from(err: crate::SimpleError) -> Self {
        match err {
            crate::SimpleError::Json(err) => Self::Parse(err),
            err @ (crate::SimpleError::UnsupportedApiVersion(_)
            | crate::SimpleError::InvalidApiVersion(_)
            | crate::SimpleError::InvalidProjectStatus(_)
            | crate::SimpleError::Html(_)) => Self::Simple(err),
        }
    }
}

impl From<upload::UploadStoreError> for CacheError {
    fn from(err: upload::UploadStoreError) -> Self {
        match err {
            upload::UploadStoreError::Meta(err) => Self::Meta(err),
            upload::UploadStoreError::Blob(err) => Self::Blob(err),
            upload::UploadStoreError::Parse(err) => Self::Parse(err),
            upload::UploadStoreError::FileExists(filename) => Self::FileExists(filename),
        }
    }
}

/// A browse method that surfaces a user-visible message maps a cache fault through `?` to its
/// [`user_message`](CacheError::user_message), so the call site carries no error-mapping closure.
impl From<CacheError> for String {
    fn from(err: CacheError) -> Self {
        err.user_message()
    }
}

impl CacheError {
    /// Error text safe for user-visible responses, without upstream URLs or credentials.
    #[must_use]
    pub fn user_message(&self) -> String {
        match self {
            Self::Meta(err) => format!("metadata store error: {err}"),
            Self::Blob(err) => format!("blob store error: {err}"),
            Self::Upstream(err) => err.user_message(),
            Self::Parse(err) => format!("simple API document could not be parsed: {err}"),
            Self::Simple(err) => format!("unsupported simple API response: {err}"),
            Self::Archive(err) => err.to_string(),
            Self::Unavailable => "upstream is unavailable and no cached page exists".to_owned(),
            Self::OfflineMissing(target) => format!("offline mode has no cached {target}"),
            Self::NotVolatile => "index is not volatile; delete is disabled".to_owned(),
            Self::FileNotFound => "no matching cached file or upstream source was found".to_owned(),
            Self::FileExists(filename) => format!("file {filename:?} already exists with different content"),
            Self::MissingSha256(filename) => format!("uploaded file {filename:?} has no sha256 hash"),
            Self::NoPromotableFiles {
                source_index,
                project,
                version,
            } => {
                format!("no uploaded files on source {source_index:?} match project {project:?} version {version:?}")
            }
            Self::Stream(err) => format!("file stream failed: {err}"),
            Self::RateLimited { retry_after } => format!("rate limit exceeded; retry after {retry_after} seconds"),
            Self::Policy(err) => err.reason.to_string(),
        }
    }
}

/// The per-page lock concurrent cache misses share.
pub(crate) fn flight_gate(state: &ServingState, key: &str) -> peryx_index::serving::FlightGate {
    peryx_index::serving::flight_gate(&state.cache.inflight, key)
}

/// Release a single-flight hold.
fn release_flight(state: &ServingState, key: &str, guard: peryx_index::serving::FlightGuard) {
    peryx_index::serving::release_flight(&state.cache.inflight, key, guard);
}

/// The stored cached record for `key`, or `None` when there is none or when its bytes no longer decode.
///
/// Corrupt bytes (a torn write, a format from a version that never shipped) leave a cache entry the
/// read-through path cannot use. Treating it as a miss lets the caller refetch and overwrite it, so one
/// bad row self-heals rather than wedging a project into permanent `500`s. A genuine store failure
/// still propagates.
fn cached_record(state: &ServingState, key: &str) -> Result<Option<CachedIndex>, CacheError> {
    match state.meta.get_index(key) {
        Err(peryx_storage::meta::MetaError::Decode(err)) => {
            tracing::warn!(key, %err, "cached index record is undecodable; refetching");
            Ok(None)
        }
        other => Ok(other?),
    }
}

/// The cached raw page, when it is still within its freshness window: upstream's `Cache-Control`
/// lifetime when it granted one, the configured fallback otherwise.
pub(crate) fn fresh_cached(state: &ServingState, key: &str) -> Result<Option<CachedIndex>, CacheError> {
    let now = (state.clock)();
    match cached_record(state, key)? {
        Some(record) if now - record.fetched_at_unix < freshness(state, &record) => Ok(Some(record)),
        _ => Ok(None),
    }
}

/// The cached raw page that is past its freshness window but still inside the stale bound, so it can
/// answer a request now while a background task revalidates it.
///
/// Reached only after [`fresh_cached`] has returned `None`, so a present record is already stale and
/// the bound is all that is left to check. `None` when nothing is cached, its bytes no longer decode,
/// or the copy has aged past `max_stale_secs` — a miss hard enough to fetch synchronously instead.
pub(crate) fn stale_servable(state: &ServingState, key: &str) -> Result<Option<CachedIndex>, CacheError> {
    Ok(cached_record(state, key)?.filter(|record| servable_stale(state, record)))
}

/// A record's freshness lifetime in seconds.
const fn freshness(state: &ServingState, record: &CachedIndex) -> i64 {
    freshness_secs(state.ttl_secs, record.fresh_secs)
}

/// The hot-cache variant holding a project's PEP 691 JSON page.
pub(crate) const SIMPLE_JSON: &str = "simple.json";
/// The hot-cache variant holding a project's PEP 503 HTML page.
pub const SIMPLE_HTML: &str = "simple.html";
/// The hot-cache variant holding a project's legacy JSON, whose release form carries its version.
pub const LEGACY_JSON: &str = "legacy.json";

/// When a rendered representation of `project` may be cached, and until when.
///
/// A rendered page is only safe to keep while the page it was rendered from is itself fresh, so the
/// expiry is that page's, never a new one. `None` means do not cache: the index is not a proxy holding
/// a cached page, or its policy filters what a project serves and the filtering is not in this key.
///
/// # Errors
/// Returns a store error when the cached page cannot be read.
pub fn rendered_expiry(state: &ServingState, index: &Index, project: &str) -> Result<Option<i64>, CacheError> {
    if index.policy.active() || !matches!(index.kind, IndexKind::Cached { .. }) {
        return Ok(None);
    }
    let key = format!("{}/{project}", index.name);
    Ok(fresh_cached(state, &key)?.map(|record| record.fetched_at_unix + freshness(state, &record)))
}

/// Whether a page past its freshness window may still answer while the upstream cannot be reached.
///
/// Serving something old beats serving nothing while an upstream reboots, but only for a while: a
/// cache that answers with whatever it last saw, forever, has stopped being a cache and started being
/// a fork. `max_stale_secs` bounds the outage a stale page papers over. `0` removes the bound, which
/// is what an operator deliberately mirroring an unreliable upstream asks for.
pub(crate) fn servable_stale(state: &ServingState, record: &CachedIndex) -> bool {
    peryx_index::serving::within_stale_bound(
        (state.clock)(),
        state.max_stale_secs,
        record.fetched_at_unix,
        freshness(state, record),
    )
}

/// How long a page stays fresh: the lifetime upstream granted, never longer than the configured one.
///
/// `Cache-Control` is the upstream's opinion, and an upstream — or any CDN that fronts it — answering
/// `max-age=31536000` would otherwise pin a page for a year with no revalidation. `ttl_secs` is both
/// the fallback when no lifetime is granted and the ceiling when too much is: a shorter upstream
/// lifetime is honoured, a longer one is not.
pub(crate) const fn freshness_secs(ttl_secs: i64, fresh_secs: Option<i64>) -> i64 {
    match fresh_secs {
        Some(granted) if granted < ttl_secs => granted,
        _ => ttl_secs,
    }
}

/// The route a cached index's pages are attributed to in metrics.
fn mirror_route(state: &ServingState, name: &str) -> String {
    state
        .indexes
        .iter()
        .find(|index| index.name == name)
        .map(|index| index.route.clone())
        .expect("events are recorded only for resolved mirrors")
}

fn project_negative_key(key: &str) -> String {
    format!("project\0{key}")
}

async fn upstream_permit(state: &ServingState, name: &str) -> Result<UpstreamPermit, CacheError> {
    state
        .upstream_limits
        .acquire(name)
        .await
        .map_err(|limited| CacheError::RateLimited {
            retry_after: limited.retry_after,
        })
}

pub(crate) fn is_json(content_type: Option<&str>) -> bool {
    // Legacy records carry no content type and hold JSON documents.
    content_type.is_none_or(|content_type| content_type.contains("json"))
}

fn supports_generated_metadata(filename: &str) -> bool {
    is_wheel(filename) || is_tar_gz(filename)
}

fn is_wheel(filename: &str) -> bool {
    std::path::Path::new(filename)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("whl"))
}

fn is_tar_gz(filename: &str) -> bool {
    filename
        .get(filename.len().saturating_sub(7)..)
        .is_some_and(|suffix| suffix.eq_ignore_ascii_case(".tar.gz"))
}

fn source_client(
    state: &ServingState,
    source: &str,
    upstream: Option<&str>,
) -> Result<(UpstreamClient, bool), CacheError> {
    let (client, offline) = state
        .indexes
        .iter()
        .find(|index| index.name == source)
        .and_then(|index| match &index.kind {
            IndexKind::Cached { client, offline } => Some((client.clone(), *offline)),
            IndexKind::Hosted { .. } | IndexKind::Virtual { .. } => None,
        })
        .ok_or(CacheError::FileNotFound)?;
    let Some(upstream) = upstream else {
        return Ok((client, offline));
    };
    let client = state
        .upstream_routes
        .get(source)
        .and_then(|router| router.source(upstream))
        .map(|source| source.client().clone())
        .ok_or(CacheError::FileNotFound)?;
    Ok((client, offline))
}

fn source_artifact_client(
    state: &ServingState,
    source: &str,
    upstream: Option<&str>,
) -> Result<(ArtifactClient, bool), CacheError> {
    let (client, offline) = source_client(state, source, None)?;
    let Some(upstream) = upstream else {
        return Ok((ArtifactClient::from(client), offline));
    };
    let artifacts = state
        .upstream_routes
        .get(source)
        .and_then(|router| router.source(upstream))
        .map(|source| source.artifacts().clone())
        .ok_or(CacheError::FileNotFound)?;
    Ok((artifacts, offline))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_error_converts_to_its_user_message_string() {
        assert_eq!(
            String::from(CacheError::Unavailable),
            "upstream is unavailable and no cached page exists"
        );
    }

    #[test]
    fn test_cache_error_archive_message_is_user_visible() {
        assert_eq!(
            CacheError::Archive(crate::archive::ArchiveError::Unsupported).user_message(),
            "unsupported archive type; accepted formats are .whl, .zip, .egg, .tar, .tar.gz, and .tgz"
        );
    }

    #[test]
    fn test_cache_error_maps_upload_store_errors() {
        let err = upload::UploadStoreError::Meta(peryx_storage::meta::MetaError::Decode(
            serde_json::from_str::<serde_json::Value>("{").unwrap_err(),
        ));
        assert!(matches!(CacheError::from(err), CacheError::Meta(_)));

        let err = upload::UploadStoreError::Blob(peryx_storage::blob::BlobError::not_found(
            &peryx_storage::blob::Digest::of(b"missing"),
        ));
        assert!(matches!(CacheError::from(err), CacheError::Blob(_)));
    }
}
