use std::collections::HashMap;
use std::ops::Deref;
use std::sync::Arc;

use peryx_core::Ecosystem;

use peryx_search::{IndexerCtx, SearchCtx};

use super::app::{AppState, ServingState};

#[derive(Default)]
struct IndexSummaries {
    summaries: HashMap<String, crate::serving::IndexSummary>,
    failures: HashMap<Ecosystem, String>,
}

impl Deref for IndexSummaries {
    type Target = HashMap<String, crate::serving::IndexSummary>;

    fn deref(&self) -> &Self::Target {
        &self.summaries
    }
}

impl AsRef<HashMap<Ecosystem, String>> for IndexSummaries {
    fn as_ref(&self) -> &HashMap<Ecosystem, String> {
        &self.failures
    }
}

impl ServingState {
    #[must_use]
    pub fn indexer_ctx(&self) -> IndexerCtx<'_> {
        IndexerCtx {
            indexes: &self.indexes,
            meta: &self.meta,
            blobs: &self.blobs,
        }
    }

    pub(crate) fn install_plugin_service<T: Send + Sync + 'static>(&mut self, service: Arc<T>) {
        self.plugin_services.insert(std::any::TypeId::of::<T>(), service);
    }
}

impl AppState {
    pub fn capability_install_context(&mut self) -> crate::serving::CapabilityInstallContext<'_> {
        crate::serving::CapabilityInstallContext::new(
            &mut self.drivers,
            &mut self.protocols,
            &mut self.absolute_prefixes,
            &mut self.rate_limit_principals,
            &mut self.client_discovery,
        )
    }

    #[must_use]
    pub fn recognizes_index_credential(&self, authorization: &str) -> bool {
        self.drivers
            .index_credentials()
            .any(|driver| driver.recognizes(authorization))
    }

    /// # Errors
    /// Returns a denial when the index has no credential driver or authorization fails.
    pub fn authorize_index_credential(
        &self,
        index: &peryx_index::Index,
        authorization: Option<&str>,
        action: peryx_identity::Action,
    ) -> Result<(), peryx_identity::Denial> {
        self.drivers
            .get_index_credentials(&index.ecosystem)
            .ok_or(peryx_identity::Denial::Unauthenticated)?
            .authorize(index, authorization, action, (self.serving.clock)())
    }

    pub fn register_lexicon(&mut self, ecosystem: Ecosystem, lexicon: &'static peryx_core::Lexicon) {
        self.lexicons.register(ecosystem, lexicon);
    }

    pub fn register_idle_reclaimer(&mut self, ecosystem: Ecosystem, driver: Arc<dyn crate::serving::IdleReclaimer>) {
        self.idle_reclaimers.insert(ecosystem, driver);
    }

    pub fn register_intent_finalizer(
        &mut self,
        ecosystem: Ecosystem,
        driver: Arc<dyn crate::serving::IntentFinalizer>,
    ) {
        self.intent_finalizers.insert(ecosystem, driver);
    }

    pub fn register_cache_refresher(&mut self, ecosystem: Ecosystem, driver: Arc<dyn crate::serving::CacheRefresher>) {
        self.cache_refreshers.insert(ecosystem, driver);
    }

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

    pub fn register_rate_limit_principal(
        &mut self,
        ecosystem: Ecosystem,
        principal: &'static dyn crate::serving::RateLimitPrincipal,
    ) {
        self.rate_limit_principals.insert(ecosystem, principal);
    }

    pub fn register_client_discovery(
        &mut self,
        ecosystem: Ecosystem,
        discovery: &'static dyn crate::serving::ClientDiscovery,
    ) {
        self.client_discovery.insert(ecosystem, discovery);
    }

    #[must_use]
    pub fn rate_limit_principal_for(
        &self,
        ecosystem: &Ecosystem,
    ) -> Option<&'static dyn crate::serving::RateLimitPrincipal> {
        self.rate_limit_principals.get(ecosystem).copied()
    }

    #[must_use]
    pub fn client_discovery_for(&self, ecosystem: &Ecosystem) -> Option<&'static dyn crate::serving::ClientDiscovery> {
        self.client_discovery.get(ecosystem).copied()
    }

    #[must_use]
    pub fn mirror_driver_for(&self, ecosystem: &Ecosystem) -> Option<&Arc<dyn crate::serving::MirrorDriver>> {
        self.mirror_drivers.get(ecosystem)
    }

    /// Keep invalidation state in [`SearchIndex`](peryx_search::SearchIndex); borrow only indexer
    /// data and vocabularies for each request.
    #[must_use]
    pub fn search_ctx(&self) -> SearchCtx<'_> {
        SearchCtx {
            indexer: self.serving.indexer_ctx(),
            lexicons: &self.lexicons,
        }
    }

    /// Register a neutral driver that contributes capabilities without serving protocol routes.
    pub fn register_driver(&mut self, driver: Arc<dyn crate::serving::EcosystemDriver>) {
        self.drivers.insert(driver);
    }

    pub fn register_capabilities(&mut self, register: impl FnOnce(&mut dyn crate::serving::CapabilityRegistrar)) {
        register(&mut self.drivers);
    }

    /// # Errors
    /// Returns an error after request-serving code has cloned the serving state.
    pub fn auth_install_context(&mut self) -> Result<crate::serving::AuthInstallContext<'_>, String> {
        let Self {
            serving, http_routes, ..
        } = self;
        let serving = Arc::get_mut(serving).ok_or_else(|| "serving state is already shared".to_owned())?;
        Ok(crate::serving::AuthInstallContext::new(serving, http_routes))
    }

    /// # Errors
    /// Returns an error after request-serving code has cloned the serving state.
    pub fn runtime_install_context(&mut self) -> Result<crate::serving::RuntimeInstallContext<'_>, String> {
        let Self {
            serving,
            protocols,
            absolute_prefixes,
            idle_reclaimers,
            intent_finalizers,
            cache_refreshers,
            mirror_drivers,
            lexicons,
            http_routes,
            ..
        } = self;
        let serving = Arc::get_mut(serving).ok_or_else(|| "serving state is already shared".to_owned())?;
        Ok(crate::serving::RuntimeInstallContext::new(
            crate::serving::RuntimeInstallDependencies {
                serving,
                protocols,
                absolute_prefixes,
                idle_reclaimers,
                intent_finalizers,
                cache_refreshers,
                mirror_drivers,
                lexicons,
                http_routes,
            },
        ))
    }

    pub(crate) fn protocol_for(&self, ecosystem: &Ecosystem) -> Option<&crate::serving::ProtocolDriver> {
        self.protocols.get(ecosystem)
    }

    /// # Errors
    /// Returns an error after request-serving code has cloned the serving state.
    pub fn distributed_install_context(&mut self) -> Result<crate::serving::DistributedInstallContext<'_>, String> {
        let Self {
            serving,
            protocols,
            absolute_prefixes,
            idle_reclaimers,
            intent_finalizers,
            cache_refreshers,
            replicated_apply_drivers,
            mirror_drivers,
            lexicons,
            http_routes,
            ..
        } = self;
        let serving = Arc::get_mut(serving).ok_or_else(|| "serving state is already shared".to_owned())?;
        Ok(crate::serving::DistributedInstallContext::new(
            crate::serving::RuntimeInstallContext::new(crate::serving::RuntimeInstallDependencies {
                serving,
                protocols,
                absolute_prefixes,
                idle_reclaimers,
                intent_finalizers,
                cache_refreshers,
                mirror_drivers,
                lexicons,
                http_routes,
            }),
            replicated_apply_drivers,
        ))
    }

    /// # Errors
    /// Returns an error after request-serving code has cloned the serving state.
    pub fn register_protocol(
        &mut self,
        protocol: crate::serving::ProtocolDriver,
        indexer: Arc<dyn peryx_search::SearchDocumentProvider>,
    ) -> Result<(), String> {
        let ecosystem = protocol.ecosystem();
        self.absolute_prefixes
            .retain(|(_, registered)| registered.ecosystem() != ecosystem);
        if let Some(driver) = protocol.absolute() {
            self.absolute_prefixes
                .extend(driver.prefixes().iter().map(|&prefix| (prefix, Arc::clone(driver))));
        }
        self.drivers.insert(protocol.driver_arc());
        self.protocols.insert(ecosystem, protocol);
        Arc::get_mut(&mut self.serving)
            .ok_or_else(|| "serving state is already shared".to_owned())?
            .search
            .add_indexer(indexer);
        Ok(())
    }

    #[must_use]
    pub fn driver_for(&self, ecosystem: &Ecosystem) -> Option<&Arc<dyn crate::serving::EcosystemDriver>> {
        self.drivers.get(ecosystem)
    }

    #[must_use]
    pub fn indexed_driver_for(&self, ecosystem: &Ecosystem) -> Option<&Arc<dyn crate::serving::IndexedProtocolDriver>> {
        self.protocols.get(ecosystem)?.indexed()
    }

    #[must_use]
    pub fn index_summaries(
        &self,
        recent_limit: usize,
    ) -> impl Deref<Target = HashMap<String, crate::serving::IndexSummary>> + AsRef<HashMap<Ecosystem, String>> {
        let mut by_ecosystem: HashMap<Ecosystem, Vec<String>> = HashMap::new();
        for index in &self.serving.indexes {
            by_ecosystem
                .entry(index.ecosystem.clone())
                .or_default()
                .push(index.name.clone());
        }
        let mut result = IndexSummaries::default();
        for (ecosystem, names) in by_ecosystem {
            let Some(driver) = self.drivers.get_index_summary(&ecosystem) else {
                continue;
            };
            match driver.summarize_indexes(&self.serving.meta, &names, recent_limit) {
                Ok(summaries) => result.summaries.extend(summaries),
                Err(error) => {
                    result.failures.insert(ecosystem, error);
                }
            }
        }
        result
    }

    pub fn drivers(&self) -> impl Iterator<Item = &Arc<dyn crate::serving::EcosystemDriver>> {
        self.drivers.present()
    }

    #[must_use]
    pub const fn driver_set(&self) -> &crate::DriverSet {
        &self.drivers
    }

    pub fn idle_reclaimers(&self) -> impl Iterator<Item = (&Ecosystem, &Arc<dyn crate::serving::IdleReclaimer>)> {
        self.idle_reclaimers.iter()
    }

    pub fn intent_finalizers(&self) -> impl Iterator<Item = (&Ecosystem, &Arc<dyn crate::serving::IntentFinalizer>)> {
        self.intent_finalizers.iter()
    }

    pub fn cache_refreshers(&self) -> impl Iterator<Item = (&Ecosystem, &Arc<dyn crate::serving::CacheRefresher>)> {
        self.cache_refreshers.iter()
    }

    pub fn replicated_apply_drivers(&self) -> impl Iterator<Item = &Arc<dyn crate::serving::ReplicatedApplyDriver>> {
        self.replicated_apply_drivers.values()
    }

    #[must_use]
    pub const fn has_any_driver(&self) -> bool {
        !self.drivers.is_empty()
    }

    /// Unique access to the serving state during build, before any handler holds a clone. Installing
    /// an ecosystem's indexer mutates the search index, which lives behind the shared `Arc`; this is
    /// sound only while that `Arc` is still uniquely owned, which it is until the router wraps it.
    fn serving_mut(&mut self) -> Result<&mut ServingState, String> {
        Arc::get_mut(&mut self.serving).ok_or_else(|| "serving state is already shared".to_owned())
    }

    /// # Errors
    /// Returns an error after request-serving code has cloned the serving state.
    pub fn set_read_only(&mut self, read_only: bool) -> Result<(), String> {
        self.serving_mut()?.read_only = read_only;
        Ok(())
    }

    #[must_use]
    pub fn absolute_driver_for_path(&self, path: &str) -> Option<&Arc<dyn crate::serving::AbsoluteProtocolDriver>> {
        self.absolute_prefixes.iter().find_map(|(prefix, driver)| {
            path.strip_prefix(prefix)
                .is_some_and(|suffix| suffix.is_empty() || prefix.ends_with('/') || suffix.starts_with('/'))
                .then_some(driver)
        })
    }

    pub fn absolute_mounts(
        &self,
    ) -> impl Iterator<Item = (&'static str, &Arc<dyn crate::serving::AbsoluteProtocolDriver>)> {
        self.absolute_prefixes.iter().map(|(prefix, driver)| (*prefix, driver))
    }

    /// The driver serving the ecosystem named `ecosystem`, so `/+api` renders that index's setup.
    #[must_use]
    pub fn driver_for_name(&self, ecosystem: &str) -> Option<&Arc<dyn crate::serving::EcosystemDriver>> {
        self.drivers().find(|driver| driver.ecosystem().as_str() == ecosystem)
    }

    pub fn set_openapi(&mut self, openapi: impl Into<Arc<str>>) {
        self.openapi = openapi.into();
    }

    #[must_use]
    pub fn openapi(&self) -> &str {
        &self.openapi
    }

    /// Install the token realm's signing key and how long its tokens live. The binary calls this once
    /// at startup when a signing key is configured; without it the realm stays unbuilt and an ecosystem
    /// serves Basic-only auth.
    /// # Errors
    /// Returns an error after request-serving code has cloned the serving state.
    pub fn set_token_realm(&mut self, signer: peryx_identity::Signer, ttl_secs: i64) -> Result<(), String> {
        let serving = self.serving_mut()?;
        serving.signer = Some(signer);
        serving.token_ttl_secs = ttl_secs;
        Ok(())
    }

    /// # Errors
    /// Returns an error after request-serving code has cloned the serving state.
    pub fn register_plugin_service<T: Send + Sync + 'static>(&mut self, service: Arc<T>) -> Result<(), String> {
        self.serving_mut()?.install_plugin_service(service);
        Ok(())
    }

    pub fn register_http_routes(&mut self, routes: Arc<dyn super::app::HttpRoutes>) {
        self.http_routes.push(routes);
    }

    /// # Errors
    /// Returns an error after request-serving code has cloned the serving state.
    pub fn install_distributed_availability(
        &mut self,
        runtime: peryx_ha::AvailabilityStateInstall,
    ) -> Result<(), String> {
        self.serving_mut()?.availability.distributed = Some(Box::new(super::app::DistributedAvailability::new(
            runtime.role,
            runtime.topology,
            runtime.blobs,
            runtime.analytics,
            runtime.capabilities,
            runtime.authority_drainer,
            runtime.operations,
        )));
        Ok(())
    }

    /// # Errors
    /// Returns an error after request-serving code has cloned the serving state.
    pub fn set_ldap_logins(
        &mut self,
        services: impl IntoIterator<Item = peryx_identity::LdapLoginService<peryx_storage::meta::MetaStore>>,
    ) -> Result<(), String> {
        let mut installed = HashMap::new();
        for service in services {
            installed.insert(service.id().to_string(), Arc::new(service));
        }
        self.serving_mut()?.ldap_logins = installed;
        Ok(())
    }

    /// # Errors
    /// Returns an error after request-serving code has cloned the serving state.
    pub fn set_oidc_logins(
        &mut self,
        services: impl IntoIterator<Item = peryx_identity::OidcLoginService<peryx_storage::meta::MetaStore>>,
    ) -> Result<(), String> {
        let mut installed = HashMap::new();
        for service in services {
            installed.insert(service.id().to_string(), Arc::new(service));
        }
        self.serving_mut()?.oidc_logins = installed;
        Ok(())
    }

    /// # Errors
    /// Returns an error after request-serving code has cloned the serving state.
    pub fn set_session_sealer(&mut self, sealer: peryx_identity::SessionSealer) -> Result<(), String> {
        self.serving_mut()?.session_sealer = Some(Arc::new(sealer));
        Ok(())
    }
}

#[cfg(test)]
#[path = "../../tests/unit/state/registry/tests.rs"]
mod tests;
