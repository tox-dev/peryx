//! The in-memory caches an `AppState` serves from: the transformed-page cache, the negative cache,
//! and the mutation epoch that retires them.

use bytes::Bytes;

use super::app::AppState;

impl AppState {
    /// A hot-cache entry that is still within its freshness window; expired entries miss.
    #[must_use]
    pub fn hot_fresh(&self, key: &str) -> Option<Bytes> {
        let (expires_at, bytes) = self.hot.get(key)?;
        ((self.clock)() < expires_at).then_some(bytes)
    }

    /// The hot-cache key for one representation of a page as served on `route` right now.
    ///
    /// `variant` separates the representations a page has: the same project answers with PEP 691 JSON,
    /// PEP 503 HTML, or the legacy release JSON, and they are different bytes. The epoch is what makes
    /// a mutation invalidate them all at once, since every mutation bumps it.
    #[must_use]
    pub fn hot_key(&self, route: &str, project: &str, variant: &str) -> String {
        let epoch = self.epoch.load(std::sync::atomic::Ordering::Relaxed);
        format!("{route}\u{0}{project}\u{0}{variant}\u{0}{epoch}")
    }

    /// Whether a remembered upstream miss is still inside its injected-clock expiry.
    #[must_use]
    pub fn negative_fresh(&self, key: &str) -> bool {
        match self.negative.get(key) {
            Some(expires_at) if (self.clock)() < expires_at => true,
            Some(_) => {
                self.negative.invalidate(key);
                false
            }
            None => false,
        }
    }

    /// Remember an upstream miss for `ttl_secs` according to the injected clock.
    pub fn remember_negative(&self, key: String, ttl_secs: i64) {
        self.negative.insert(key, (self.clock)() + ttl_secs);
    }

    /// Retire every hot-cache entry after a mutation (upload, yank, hide, restore, or a fresh
    /// upstream page).
    pub fn bump_epoch(&self) {
        self.epoch.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}
