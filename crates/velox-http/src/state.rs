//! Shared application state.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use velox_storage::blob::BlobStore;
use velox_storage::meta::MetaStore;
use velox_upstream::UpstreamClient;

/// A source of the current unix time, injectable so cache-freshness logic is deterministic in
/// tests.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// Everything a request handler needs. Shared as `Arc<AppState>`.
pub struct AppState {
    pub meta: MetaStore,
    pub blobs: BlobStore,
    pub upstream: UpstreamClient,
    /// The configured mirror index route prefix, for example `root/pypi`.
    pub index: String,
    /// The private upload index route prefix, for example `root/local`. Distinct from the mirror:
    /// it serves uploaded distributions rather than proxying an upstream.
    pub upload_index: String,
    /// The shared secret an upload must present as its Basic-auth password. `None` disables uploads.
    pub upload_token: Option<String>,
    /// How long a cached simple page is served before revalidating, in seconds.
    pub ttl_secs: i64,
    pub clock: Clock,
    pub requests: AtomicU64,
    /// PEP 658/714 `.metadata` sibling requests served, exposed via `/metrics`. Downstream clients
    /// only hit this when they take the metadata-only resolution fast path, so it is the server-side
    /// proof that pip and uv resolve through velox without downloading whole wheels.
    pub metadata_requests: AtomicU64,
}

impl AppState {
    /// Build the state with a system clock.
    #[must_use]
    pub fn new(config: StateConfig) -> Self {
        Self::with_clock(config, Arc::new(system_now))
    }

    /// Build the state with an injected clock.
    #[must_use]
    pub fn with_clock(config: StateConfig, clock: Clock) -> Self {
        let StateConfig {
            meta,
            blobs,
            upstream,
            index,
            upload_index,
            upload_token,
            ttl_secs,
        } = config;
        Self {
            meta,
            blobs,
            upstream,
            index,
            upload_index,
            upload_token,
            ttl_secs,
            clock,
            requests: AtomicU64::new(0),
            metadata_requests: AtomicU64::new(0),
        }
    }

    /// Whether `user/index` addresses the mirror index.
    #[must_use]
    pub fn is_mirror(&self, user: &str, index: &str) -> bool {
        self.index == format!("{user}/{index}")
    }

    /// Whether `user/index` addresses the private upload index.
    #[must_use]
    pub fn is_upload(&self, user: &str, index: &str) -> bool {
        self.upload_index == format!("{user}/{index}")
    }

    /// The stored index key if `user/index` is either the mirror or the upload index; `None`
    /// otherwise. Used where the two are served the same way (project list, file download).
    #[must_use]
    pub fn resolve_index(&self, user: &str, index: &str) -> Option<String> {
        let key = format!("{user}/{index}");
        (key == self.index || key == self.upload_index).then_some(key)
    }
}

/// The stored fields of an [`AppState`], grouped so the constructor takes one argument instead of a
/// long positional list.
pub struct StateConfig {
    pub meta: MetaStore,
    pub blobs: BlobStore,
    pub upstream: UpstreamClient,
    pub index: String,
    pub upload_index: String,
    pub upload_token: Option<String>,
    pub ttl_secs: i64,
}

fn system_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}
