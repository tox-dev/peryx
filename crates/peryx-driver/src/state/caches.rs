//! The `AppState` cache accessors, delegating to the role engine's serving cache with the process
//! clock supplied.

use bytes::Bytes;
use peryx_storage::meta::MetaError;

use super::app::ServingState;
use super::derived_views::{ReadableFrontier, readable_frontier as compute_readable_frontier};

impl ServingState {
    /// A hot-cache entry that is still within its freshness window; expired entries miss.
    #[must_use]
    pub fn hot_fresh(&self, key: &str) -> Option<Bytes> {
        self.cache.hot_fresh(key, (self.clock)())
    }

    /// A fresh hot-cache entry and the source revision attached by its driver.
    #[must_use]
    pub fn hot_fresh_versioned(&self, key: &str) -> Option<(Bytes, Option<u64>)> {
        self.cache.hot_fresh_versioned(key, (self.clock)())
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

    /// Invalidate one project's rendered pages and search documents after a project mutation. The
    /// search side refreshes only this project's documents, so a mutation no longer rebuilds the whole
    /// index on the next search.
    pub fn invalidate_project(&self, project: &str) {
        self.cache.invalidate_hot(project);
        self.search.invalidate_project(project);
    }

    /// Retire one project's rendered pages without touching the search epoch. A replica applying a page
    /// rebuilds that project's search document itself through the scoped path, so it invalidates the hot
    /// pages here and advances the search view without the global reindex the epoch bump would schedule.
    pub fn invalidate_hot_pages(&self, project: &str) {
        self.cache.invalidate_hot(project);
    }

    /// Preserve rendered page caches when the ecosystem reports no project-level invalidation.
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
