//! What an `AppState` has installed: each ecosystem's serving driver, its search indexer, its
//! vocabulary, and the assembled `OpenAPI` document.

use std::sync::Arc;

use peryx_core::Ecosystem;

use peryx_search::{IndexerCtx, SearchCtx};

use super::app::AppState;

impl AppState {
    /// Register an ecosystem's user-facing vocabulary; its driver calls this at install time.
    pub fn register_lexicon(&mut self, ecosystem: Ecosystem, lexicon: &'static peryx_core::Lexicon) {
        self.lexicons.register(ecosystem, lexicon);
    }

    /// The user-facing vocabulary for `ecosystem`, or peryx's neutral words if none is registered.
    #[must_use]
    pub fn lexicon(&self, ecosystem: Ecosystem) -> &'static peryx_core::Lexicon {
        self.lexicons.get(ecosystem)
    }

    /// The stores and indexes an ecosystem's search indexer walks.
    #[must_use]
    pub fn indexer_ctx(&self) -> IndexerCtx<'_> {
        IndexerCtx {
            indexes: &self.indexes,
            meta: &self.meta,
            blobs: &self.blobs,
        }
    }

    /// What one search request reads from this state: the indexers' stores, the mutation epoch that
    /// decides whether the derived index is stale, and the registered vocabularies.
    #[must_use]
    pub fn search_ctx(&self) -> SearchCtx<'_> {
        SearchCtx {
            indexer: self.indexer_ctx(),
            epoch: self.epoch.load(std::sync::atomic::Ordering::Relaxed),
            lexicons: &self.lexicons,
        }
    }

    /// Register a route-mounted ecosystem's serving driver and its search indexer. Each driver's own
    /// [`ecosystem`](crate::serving::EcosystemServing::ecosystem) picks its slot, so installing one
    /// never displaces another.
    pub fn register_ecosystem(
        &mut self,
        serving: Arc<dyn crate::serving::EcosystemServing>,
        indexer: Arc<dyn peryx_search::PackageIndexer>,
    ) {
        let slot = serving.ecosystem().slot();
        self.serving[slot] = Some(serving);
        self.search.add_indexer(indexer);
    }

    /// The route-mounted driver serving `ecosystem`, or `None` when none is installed for it.
    #[must_use]
    pub fn serving_for(&self, ecosystem: Ecosystem) -> Option<&Arc<dyn crate::serving::EcosystemServing>> {
        self.serving[ecosystem.slot()].as_ref()
    }

    /// The route-mounted driver that would serve `path`, found by resolving the index it addresses.
    ///
    /// `path` is a request URI path, so it carries a leading slash; index routes do not.
    #[must_use]
    pub fn serving_for_path(&self, path: &str) -> Option<&Arc<dyn crate::serving::EcosystemServing>> {
        let (position, _) = self.resolve_position(path.trim_start_matches('/'))?;
        self.serving_for(self.index_at(position).ecosystem)
    }

    /// Every installed route-mounted driver, in ecosystem declaration order.
    pub fn servings(&self) -> impl Iterator<Item = &Arc<dyn crate::serving::EcosystemServing>> {
        self.serving.iter().flatten()
    }

    /// Whether any ecosystem driver at all has been wired in. A process with none serves `503` rather
    /// than quietly answering nothing.
    #[must_use]
    pub fn has_any_driver(&self) -> bool {
        self.serving.iter().any(Option::is_some) || !self.namespaces.is_empty()
    }

    /// Add another ecosystem's search indexer, composing with any already installed. An ecosystem
    /// whose serving lives in its own slot (OCI) uses this to make its packages searchable too.
    pub fn add_search_indexer(&mut self, indexer: Arc<dyn peryx_search::PackageIndexer>) {
        self.search.add_indexer(indexer);
    }

    /// Wire in a namespace ecosystem's serving driver. The binary calls this once at startup for each
    /// namespace ecosystem (OCI's `/v2/` registry) whose indexes are configured.
    pub fn register_namespace(&mut self, driver: Arc<dyn crate::serving::NamespaceServing>) {
        self.namespaces.push(driver);
    }

    /// The namespace driver that owns `path`, or `None` when the path falls under no namespace (the
    /// per-index router handles it). The first registered driver whose prefix matches wins.
    #[must_use]
    pub fn namespace_for_path(&self, path: &str) -> Option<&Arc<dyn crate::serving::NamespaceServing>> {
        self.namespaces
            .iter()
            .find(|driver| driver.prefixes().iter().any(|prefix| path.starts_with(prefix)))
    }

    /// The namespace driver serving `ecosystem`, so `/+api` renders that index's setup through it.
    #[must_use]
    pub fn namespace_for_ecosystem(&self, ecosystem: &str) -> Option<&Arc<dyn crate::serving::NamespaceServing>> {
        self.namespaces
            .iter()
            .find(|driver| driver.ecosystem().as_str() == ecosystem)
    }

    /// Install the assembled `OpenAPI` document the `/api-docs/openapi.json` endpoint serves. The
    /// binary builds it from each ecosystem driver's paths and calls this once at startup.
    pub fn set_openapi(&mut self, openapi: impl Into<Arc<str>>) {
        self.openapi = openapi.into();
    }

    /// The installed `OpenAPI` document served at `/api-docs/openapi.json`.
    #[must_use]
    pub fn openapi(&self) -> &str {
        &self.openapi
    }
}
