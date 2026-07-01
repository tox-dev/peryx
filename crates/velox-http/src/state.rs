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
    /// The configured index route prefix, for example `root/pypi`.
    pub index: String,
    /// How long a cached simple page is served before revalidating, in seconds.
    pub ttl_secs: i64,
    pub clock: Clock,
    pub requests: AtomicU64,
}

impl AppState {
    /// Build the state with a system clock.
    #[must_use]
    pub fn new(meta: MetaStore, blobs: BlobStore, upstream: UpstreamClient, index: String, ttl_secs: i64) -> Self {
        Self::with_clock(meta, blobs, upstream, index, ttl_secs, Arc::new(system_now))
    }

    /// Build the state with an injected clock.
    #[must_use]
    pub fn with_clock(
        meta: MetaStore,
        blobs: BlobStore,
        upstream: UpstreamClient,
        index: String,
        ttl_secs: i64,
        clock: Clock,
    ) -> Self {
        Self {
            meta,
            blobs,
            upstream,
            index,
            ttl_secs,
            clock,
            requests: AtomicU64::new(0),
        }
    }

    /// Whether `user/index` addresses the configured index.
    #[must_use]
    pub fn matches_index(&self, user: &str, index: &str) -> bool {
        self.index == format!("{user}/{index}")
    }
}

fn system_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}
