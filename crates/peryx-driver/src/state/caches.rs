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

    /// Retire every hot-cache entry after a mutation (upload, yank, hide, restore, or a fresh
    /// upstream page).
    pub fn bump_epoch(&self) {
        self.cache.bump_epoch();
    }
}
