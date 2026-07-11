//! What an `AppState` has installed: each ecosystem's serving driver, its search indexer, its
//! vocabulary, and the assembled `OpenAPI` document.

use std::collections::HashMap;
use std::sync::Arc;

use peryx_core::Ecosystem;

use peryx_search::{IndexerCtx, SearchCtx};

use super::app::{AppState, ServingState};

impl ServingState {
    /// The stores and indexes an ecosystem's search indexer walks.
    #[must_use]
    pub fn indexer_ctx(&self) -> IndexerCtx<'_> {
        IndexerCtx {
            indexes: &self.indexes,
            meta: &self.meta,
            blobs: &self.blobs,
        }
    }
}

impl AppState {
    /// Register an ecosystem's user-facing vocabulary; its driver calls this at install time.
    pub fn register_lexicon(&mut self, ecosystem: Ecosystem, lexicon: &'static peryx_core::Lexicon) {
        self.lexicons.register(ecosystem, lexicon);
    }

    /// What one search request reads from this state: the indexers' stores, the mutation epoch that
    /// decides whether the derived index is stale, and the registered vocabularies.
    #[must_use]
    pub fn search_ctx(&self) -> SearchCtx<'_> {
        SearchCtx {
            indexer: self.indexer_ctx(),
            epoch: self.cache.epoch.load(std::sync::atomic::Ordering::Relaxed),
            lexicons: &self.lexicons,
        }
    }

    /// Register an ecosystem's serving driver and its search indexer. The driver's own
    /// [`ecosystem`](crate::serving::EcosystemDriver::ecosystem) picks its slot, so installing one
    /// never displaces another.
    pub fn register_ecosystem(
        &mut self,
        driver: Arc<dyn crate::serving::EcosystemDriver>,
        indexer: Arc<dyn peryx_search::PackageIndexer>,
    ) {
        let slot = driver.ecosystem().slot();
        if let crate::serving::RouteMount::Absolute(prefixes) = driver.mount() {
            self.absolute_prefixes
                .extend(prefixes.iter().map(|&prefix| (prefix, slot)));
        }
        self.drivers[slot] = Some(driver);
        self.serving_mut().search.add_indexer(indexer);
    }

    /// The driver serving `ecosystem`, or `None` when none is installed for it.
    #[must_use]
    pub fn driver_for(&self, ecosystem: Ecosystem) -> Option<&Arc<dyn crate::serving::EcosystemDriver>> {
        self.drivers[ecosystem.slot()].as_ref()
    }

    /// Per-index activity (project/upload counts and recent uploads) for the status page and
    /// dashboard, keyed by index name. Configured indexes are grouped by ecosystem and each group is
    /// summarized through its own driver, so no neutral code reads a format's tables.
    #[must_use]
    pub fn index_summaries(&self, recent_limit: usize) -> HashMap<String, crate::serving::IndexSummary> {
        let mut by_ecosystem: HashMap<Ecosystem, Vec<String>> = HashMap::new();
        for index in &self.indexes {
            by_ecosystem
                .entry(index.ecosystem)
                .or_default()
                .push(index.name.clone());
        }
        let mut summaries = HashMap::new();
        for (ecosystem, names) in by_ecosystem {
            if let Some(driver) = self.driver_for(ecosystem)
                && let Ok(map) = driver.summarize_indexes(&self.meta, &names, recent_limit)
            {
                summaries.extend(map);
            }
        }
        summaries
    }

    /// The indexed-mount driver that would serve `path`, found by resolving the index it addresses.
    ///
    /// `path` is a request URI path, so it carries a leading slash; index routes do not.
    #[must_use]
    pub fn driver_for_path(&self, path: &str) -> Option<&Arc<dyn crate::serving::EcosystemDriver>> {
        let (position, _) = self.resolve_position(path.trim_start_matches('/'))?;
        self.driver_for(self.index_at(position).ecosystem)
    }

    /// Every installed driver, in ecosystem declaration order.
    pub fn drivers(&self) -> impl Iterator<Item = &Arc<dyn crate::serving::EcosystemDriver>> {
        self.drivers.iter().flatten()
    }

    /// Whether any ecosystem driver at all has been wired in. A process with none serves `503` rather
    /// than quietly answering nothing.
    #[must_use]
    pub fn has_any_driver(&self) -> bool {
        self.drivers.iter().any(Option::is_some)
    }

    /// Unique access to the serving state during build, before any handler holds a clone. Installing
    /// an ecosystem's indexer mutates the search index, which lives behind the shared `Arc`; this is
    /// sound only while that `Arc` is still uniquely owned, which it is until the router wraps it.
    fn serving_mut(&mut self) -> &mut ServingState {
        Arc::get_mut(&mut self.serving).expect("serving state is registered before it is served")
    }

    /// The absolute-mount driver that owns `path` (`OCI`'s `/v2/`), or `None` when the path falls under
    /// no such prefix and the per-index router handles it.
    #[must_use]
    pub fn absolute_driver_for_path(&self, path: &str) -> Option<&Arc<dyn crate::serving::EcosystemDriver>> {
        let slot = self
            .absolute_prefixes
            .iter()
            .find_map(|&(prefix, slot)| path.starts_with(prefix).then_some(slot))?;
        self.drivers[slot].as_ref()
    }

    /// The absolute top-level prefixes each with its driver, for the router to mount catch-alls under.
    pub fn absolute_mounts(&self) -> impl Iterator<Item = (&'static str, &Arc<dyn crate::serving::EcosystemDriver>)> {
        self.absolute_prefixes
            .iter()
            .filter_map(|&(prefix, slot)| Some((prefix, self.drivers[slot].as_ref()?)))
    }

    /// The driver serving the ecosystem named `ecosystem`, so `/+api` renders that index's setup.
    #[must_use]
    pub fn driver_for_name(&self, ecosystem: &str) -> Option<&Arc<dyn crate::serving::EcosystemDriver>> {
        self.drivers().find(|driver| driver.ecosystem().as_str() == ecosystem)
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
