use bytes::Bytes;
use peryx_storage::meta::MetaError;

use super::app::ServingState;
use super::derived_views::{ReadableFrontier, readable_frontier as compute_readable_frontier};

impl ServingState {
    #[must_use]
    pub fn hot_fresh(&self, key: &str) -> Option<Bytes> {
        self.cache.hot_fresh(key, (self.clock)())
    }

    #[must_use]
    pub fn hot_fresh_versioned(&self, key: &str) -> Option<(Bytes, Option<u64>)> {
        self.cache.hot_fresh_versioned(key, (self.clock)())
    }

    #[must_use]
    pub fn representation_key(&self, route: &str, resource: &str, representation: &str) -> String {
        self.cache.representation_key(route, resource, representation)
    }

    #[must_use]
    pub fn negative_fresh(&self, key: &str) -> bool {
        self.cache.negative_fresh(key, (self.clock)())
    }

    pub fn remember_negative(&self, key: String, ttl_secs: i64) {
        self.cache.remember_negative(key, (self.clock)() + ttl_secs);
    }

    pub fn invalidate_resource(&self, resource: &str) {
        self.cache.invalidate_resource(resource);
        self.search.invalidate_resource(resource);
    }

    /// Retire one resource's representations without touching the search epoch.
    pub fn invalidate_representations(&self, resource: &str) {
        self.cache.invalidate_resource(resource);
    }

    pub fn bump_search_epoch(&self) {
        self.search.bump_epoch();
    }

    /// The highest metadata serial a replica may expose and the view holding it back, from the
    /// authoritative serial and every required view's durable frontier. Metadata above it stays
    /// hidden until the lagging view catches up, so a read never mixes new metadata with a stale view.
    ///
    /// # Errors
    /// Returns a store error if either read fails.
    pub fn readable_frontier(&self) -> Result<ReadableFrontier, MetaError> {
        let authority = self.meta.current_serial()?;
        let frontiers = self.meta.view_frontiers()?;
        Ok(compute_readable_frontier(authority, &frontiers, &self.required_views))
    }
}

#[cfg(test)]
#[path = "../../tests/unit/state/caches/tests.rs"]
mod tests;
