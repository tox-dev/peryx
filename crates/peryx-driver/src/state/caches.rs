//! The `AppState` cache accessors, delegating to the role engine's serving cache with the process
//! clock supplied.

use bytes::Bytes;

use super::app::ServingState;

impl ServingState {
    /// A hot-cache entry that is still within its freshness window; expired entries miss.
    #[must_use]
    pub fn hot_fresh(&self, key: &str) -> Option<Bytes> {
        self.cache.hot_fresh(key, (self.clock)())
    }

    /// The hot-cache key for one representation of a page as served on `route` right now.
    #[must_use]
    pub fn hot_key(&self, route: &str, project: &str, variant: &str) -> String {
        self.cache.hot_key(route, project, variant)
    }

    /// Whether a remembered upstream miss is still inside its injected-clock expiry.
    #[must_use]
    pub fn negative_fresh(&self, key: &str) -> bool {
        self.cache.negative_fresh(key, (self.clock)())
    }

    /// Remember an upstream miss for `ttl_secs` according to the injected clock.
    pub fn remember_negative(&self, key: String, ttl_secs: i64) {
        self.cache.remember_negative(key, (self.clock)() + ttl_secs);
    }

    /// Invalidate one project's rendered pages and the search documents after a `PyPI` mutation.
    pub fn invalidate_project(&self, project: &str) {
        self.cache.invalidate_hot(project);
        self.bump_search_epoch();
    }

    /// Preserve rendered page caches across `OCI` tag mutations.
    pub fn bump_search_epoch(&self) {
        self.search.bump_epoch();
    }
}
