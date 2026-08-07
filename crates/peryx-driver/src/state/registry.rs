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

    /// Register a maintenance capability for one ecosystem.
    pub fn register_maintenance_driver(
        &mut self,
        ecosystem: Ecosystem,
        driver: Arc<dyn crate::serving::MaintenanceDriver>,
    ) {
        self.maintenance_drivers.insert(ecosystem, driver);
    }

    /// Register a replicated-view apply capability for one ecosystem.
    pub fn register_replicated_apply_driver(
        &mut self,
        ecosystem: Ecosystem,
        driver: Arc<dyn crate::serving::ReplicatedApplyDriver>,
    ) {
        self.replicated_apply_drivers.insert(ecosystem, driver);
    }

    pub fn register_mirror_driver(&mut self, ecosystem: Ecosystem, driver: Arc<dyn crate::serving::MirrorDriver>) {
        self.mirror_drivers.insert(ecosystem, driver);
    }

    #[must_use]
    pub fn mirror_driver_for(&self, ecosystem: Ecosystem) -> Option<&Arc<dyn crate::serving::MirrorDriver>> {
        self.mirror_drivers.get(&ecosystem)
    }

    /// Keep invalidation state in [`PackageSearch`](peryx_search::PackageSearch); borrow only indexer
    /// data and vocabularies for each request.
    #[must_use]
    pub fn search_ctx(&self) -> SearchCtx<'_> {
        SearchCtx {
            indexer: self.indexer_ctx(),
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
        let ecosystem = driver.ecosystem();
        if let crate::serving::RouteMount::Absolute(prefixes) = driver.mount() {
            self.absolute_prefixes
                .extend(prefixes.iter().map(|&prefix| (prefix, ecosystem)));
        }
        self.drivers.insert(ecosystem, driver);
        self.serving_mut().search.add_indexer(indexer);
    }

    /// The driver serving `ecosystem`, or `None` when none is installed for it.
    #[must_use]
    pub fn driver_for(&self, ecosystem: Ecosystem) -> Option<&Arc<dyn crate::serving::EcosystemDriver>> {
        self.drivers.get(&ecosystem)
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

    /// Every installed driver, in ecosystem declaration order.
    pub fn drivers(&self) -> impl Iterator<Item = &Arc<dyn crate::serving::EcosystemDriver>> {
        self.drivers.values()
    }

    /// Maintenance-capable drivers, in ecosystem declaration order.
    pub fn maintenance_drivers(&self) -> impl Iterator<Item = &Arc<dyn crate::serving::MaintenanceDriver>> {
        self.maintenance_drivers.values()
    }

    /// Replication-view drivers, in ecosystem declaration order.
    pub fn replicated_apply_drivers(&self) -> impl Iterator<Item = &Arc<dyn crate::serving::ReplicatedApplyDriver>> {
        self.replicated_apply_drivers.values()
    }

    /// Whether any ecosystem driver at all has been wired in. A process with none serves `503` rather
    /// than quietly answering nothing.
    #[must_use]
    pub fn has_any_driver(&self) -> bool {
        !self.drivers.is_empty()
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
        let ecosystem = self
            .absolute_prefixes
            .iter()
            .find_map(|&(prefix, ecosystem)| path.starts_with(prefix).then_some(ecosystem))?;
        self.drivers.get(&ecosystem)
    }

    /// The absolute top-level prefixes each with its driver, for the router to mount catch-alls under.
    pub fn absolute_mounts(&self) -> impl Iterator<Item = (&'static str, &Arc<dyn crate::serving::EcosystemDriver>)> {
        self.absolute_prefixes
            .iter()
            .filter_map(|&(prefix, ecosystem)| Some((prefix, self.drivers.get(&ecosystem)?)))
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

    /// Install the token realm's signing key and how long its tokens live. The binary calls this once
    /// at startup when a signing key is configured; without it the realm stays unbuilt and an ecosystem
    /// serves Basic-only auth.
    pub fn set_token_realm(&mut self, signer: peryx_identity::Signer, ttl_secs: i64) {
        let serving = self.serving_mut();
        serving.signer = Some(signer);
        serving.token_ttl_secs = ttl_secs;
    }

    /// Keep issuer clients and replay state absent until configuration enables the exchange.
    pub fn set_trusted_publishing(&mut self, runtime: impl peryx_identity::IdentityExchange + 'static) {
        self.serving_mut().trusted_publishing = Some(Arc::new(runtime));
    }

    /// Install the fixed availability topology the binary resolved from configuration, so the topology
    /// snapshot endpoint reports the group without reading configuration at request time.
    pub fn set_availability_topology(&mut self, topology: peryx_core::TopologyConfig) {
        self.serving_mut().enable_distributed().topology = topology;
    }

    /// Install the resolved hosted-write acknowledgement quorum and client deadline.
    pub fn set_write_ack(&mut self, policy: peryx_ha::DurabilityPolicy, deadline: std::time::Duration) {
        let availability = self.serving_mut().enable_distributed();
        availability.write_ack_policy = policy;
        availability.write_ack_deadline = deadline;
    }

    /// Install the same-datacenter peers a filesystem write gathers placement receipts from, so a
    /// multi-node-DC quorum resolves from real evidence rather than the local receipt alone.
    pub fn set_receipt_sources(&mut self, sources: Vec<std::sync::Arc<dyn peryx_ha::ReceiptSource + Send + Sync>>) {
        self.serving_mut().enable_distributed().receipt_sources = sources;
    }

    /// Install the eligible remote datacenters an `ha` write gathers metadata acknowledgements from, so
    /// its metadata dimension resolves from a remote commit rather than the local journal alone.
    pub fn set_remote_frontier_sources(
        &mut self,
        sources: Vec<std::sync::Arc<dyn peryx_ha::RemoteFrontierSource + Send + Sync>>,
    ) {
        self.serving_mut().enable_distributed().remote_frontier_sources = sources;
    }

    /// Install the authority role the binary resolved from the configured replication role, so the
    /// topology snapshot reports a configured primary as the writer even when it serves read-only.
    pub fn set_availability_role(&mut self, role: peryx_core::NodeRole) {
        self.serving_mut().enable_distributed().role = role;
    }

    /// Install named LDAP login services after their secrets and trust files resolve at startup.
    pub fn set_ldap_logins(
        &mut self,
        services: impl IntoIterator<Item = peryx_identity::LdapLoginService<peryx_storage::meta::MetaStore>>,
    ) {
        self.serving_mut().ldap_logins = services
            .into_iter()
            .map(|service| (service.id().to_string(), Arc::new(service)))
            .collect();
    }

    /// Install the named browser OIDC login services, replacing any set before serving started.
    pub fn set_oidc_logins(
        &mut self,
        services: impl IntoIterator<Item = peryx_identity::OidcLoginService<peryx_storage::meta::MetaStore>>,
    ) {
        self.serving_mut().oidc_logins = services
            .into_iter()
            .map(|service| (service.id().to_string(), Arc::new(service)))
            .collect();
    }

    /// Install the sealer for browser session and login-handoff cookies, replacing any set before
    /// serving started.
    pub fn set_session_sealer(&mut self, sealer: peryx_identity::SessionSealer) {
        self.serving_mut().session_sealer = Some(Arc::new(sealer));
    }
}
