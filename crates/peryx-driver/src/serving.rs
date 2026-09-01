use std::any::Any;
use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::Request;
use axum::http::{HeaderMap, Method, Uri};
use axum::response::Response;
use peryx_core::DefaultIndex;
use peryx_core::{BrowsePage, Ecosystem};

use crate::HttpRoutes;
use crate::state::{ServingState, ViewBlock};

#[derive(Debug, Clone)]
pub struct PluginIndexConfig<'a> {
    pub name: &'a str,
    pub ecosystem: Ecosystem,
    pub writable: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct PluginAuthConfig<'a> {
    pub values: &'a toml::Table,
    pub signing_key_configured: bool,
    pub token_ttl_secs: i64,
    pub indexes: &'a [PluginIndexConfig<'a>],
}

pub struct CompiledEcosystemSettings {
    ecosystem: Ecosystem,
    value: Box<dyn Any + Send + Sync>,
}

impl std::fmt::Debug for CompiledEcosystemSettings {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CompiledEcosystemSettings")
            .field("ecosystem", &self.ecosystem)
            .finish_non_exhaustive()
    }
}

impl CompiledEcosystemSettings {
    #[must_use]
    pub fn new<T: Send + Sync + 'static>(ecosystem: Ecosystem, value: T) -> Self {
        Self {
            ecosystem,
            value: Box::new(value),
        }
    }

    #[must_use]
    pub fn ecosystem(&self) -> Ecosystem {
        self.ecosystem.clone()
    }

    #[must_use]
    pub fn value<T: 'static>(&self) -> Option<&T> {
        self.value.downcast_ref()
    }
}

pub trait EcosystemRegistration: Send + Sync {
    fn ecosystem(&self) -> Ecosystem;
    fn default_indexes(&self) -> &'static [DefaultIndex];
    fn absolute_prefixes(&self) -> &'static [&'static str];
    fn webhook_events(&self) -> &'static [&'static str];
    fn driver(&self) -> ProtocolDriver;
    fn register_capabilities(&self, registrar: &mut dyn CapabilityRegistrar);
}

pub trait EcosystemAuth: Send + Sync {
    /// # Errors
    /// Returns the ecosystem's configuration error.
    fn validate(&self, config: PluginAuthConfig<'_>) -> Result<(), String>;
    /// # Errors
    /// Returns an error when authentication services cannot start.
    fn install(&self, context: &mut AuthInstallContext<'_>, values: &toml::Table) -> Result<(), String>;
}

pub trait EcosystemConfig: Send + Sync {
    /// # Errors
    /// Returns an error when the ecosystem rejects index settings.
    fn compile_index_settings(
        &self,
        name: &str,
        settings: &toml::Table,
    ) -> Result<Option<CompiledEcosystemSettings>, String>;
}

pub trait EcosystemRuntime: Send + Sync {
    /// # Errors
    /// Returns an error when local runtime services cannot start.
    fn install(
        &self,
        context: &mut RuntimeInstallContext<'_>,
        settings: &[(&str, &CompiledEcosystemSettings)],
    ) -> Result<(), String>;
}

pub trait DistributedRuntime: Send + Sync {
    /// # Errors
    /// Returns an error when distributed runtime services cannot start.
    fn install(
        &self,
        context: &mut DistributedInstallContext<'_>,
        settings: &[(&str, &CompiledEcosystemSettings)],
    ) -> Result<(), String>;
}

/// Resolves owner credentials before neutral rate-limit bucket selection.
pub trait RateLimitPrincipal: Send + Sync {
    fn resolve(&self, state: &ServingState, position: Option<usize>, headers: &HeaderMap) -> peryx_identity::Principal;
}

/// Renders client addresses without teaching core code an ecosystem protocol.
pub trait ClientDiscovery: Send + Sync {
    fn discover_index(
        &self,
        index: crate::state::IndexDescription,
        base: Option<&crate::discovery::BaseUrl>,
    ) -> serde_json::Value;

    fn client_endpoint(&self, route: &str) -> String;
}

#[async_trait]
pub trait EcosystemBrowse: Send + Sync {
    fn paths(&self) -> &'static [&'static str];
    async fn dispatch(&self, state: Arc<crate::AppState>, request: Request) -> Response;
}

pub trait EcosystemOpenApi: Send + Sync {
    fn paths(&self, paths: utoipa::openapi::PathsBuilder) -> utoipa::openapi::PathsBuilder;
}

pub trait EcosystemSnippet: Send + Sync {
    /// # Errors
    /// Returns an error when `format` is unsupported or snippet generation fails.
    fn text(
        &self,
        base: &crate::discovery::BaseUrl,
        route: &str,
        uploads: bool,
        format: &str,
    ) -> Result<Option<String>, String>;
}

#[async_trait]
pub trait IdleReclaimer: Send + Sync {
    async fn reclaim_idle(&self, state: Arc<ServingState>) -> usize;
}

#[async_trait]
pub trait IntentFinalizer: Send + Sync {
    async fn finalize_admitted(&self, state: Arc<ServingState>) -> u64;
}

#[async_trait]
pub trait CacheRefresher: Send + Sync {
    async fn refresh_stale(&self, state: Arc<ServingState>) -> Result<RefreshSweep, String>;
}

/// Replicated-view rebuild capability for replica followers.
pub trait ReplicatedApplyDriver: Send + Sync {
    /// # Errors
    /// Returns the derived view that could not apply the changes.
    fn apply_replicated_changes(&self, state: &ServingState, changed_keys: &[String]) -> Result<(), ViewBlock>;
}

pub struct JobIndexConfig<'a> {
    pub name: &'a str,
    pub ecosystem: Ecosystem,
    pub cached: bool,
    pub offline: bool,
    pub upstreams: Vec<&'a str>,
}

#[derive(Clone, Copy)]
pub struct JobConfig<'a> {
    pub kind: &'a str,
    pub settings: &'a toml::Table,
    pub indexes: &'a [JobIndexConfig<'a>],
}

pub trait JobDriver: Send + Sync {
    fn compile_job(&self, config: JobConfig<'_>) -> Option<Result<crate::jobs::PluginScheduledJob, String>>;
}

pub trait MetricsDriver: Send + Sync {
    fn metric_families(&self) -> &'static [peryx_events::metrics::MetricFamily];
}

pub trait NameDriver: Send + Sync {
    fn normalize_name(&self, name: &str) -> String;
}

pub trait PolicyDriver: Send + Sync {
    /// # Errors
    /// Returns the invalid policy setting.
    fn compile_policy(&self, policy: &toml::Table) -> Result<peryx_policy::PolicyCapabilities, String>;
}

pub trait PolicyDryRunDriver: Send + Sync {
    /// # Errors
    /// Returns an error when policy evaluation or report output fails.
    fn policy_dry_run(
        &self,
        meta: &peryx_storage::meta::MetaStore,
        indexes: &[peryx_index::Index],
        index_filter: Option<&str>,
        resource_filter: Option<&str>,
        out: &mut dyn Write,
    ) -> Result<(), String>;
}

pub trait BlobReferenceDriver: Send + Sync {
    /// # Errors
    /// Returns an error when stored references cannot be read.
    fn referenced_blob_digests(
        &self,
        meta: &peryx_storage::meta::MetaStore,
    ) -> Result<std::collections::BTreeSet<String>, String>;
}

pub trait FsckDriver: Send + Sync {
    /// # Errors
    /// Returns an error when metadata cannot be checked or repaired.
    fn fsck_metadata(
        &self,
        meta: &peryx_storage::meta::MetaStore,
        blobs: &peryx_storage::blob::BlobStorage,
        out: &mut dyn Write,
    ) -> Result<u64, String>;
}

/// One retention plan's inputs, including the stop signal its scan checks between pages.
pub struct RetentionScan<'a> {
    pub meta: &'a peryx_storage::meta::MetaStore,
    pub index: &'a str,
    pub policy: &'a peryx_policy::RetentionPolicy,
    pub now: Option<i64>,
    pub cancellation: &'a crate::ScanCancellation,
}

pub trait RetentionDriver: Send + Sync {
    /// # Errors
    /// Returns an error naming a selector the ecosystem cannot evaluate.
    fn validate_retention(&self, policy: &peryx_policy::RetentionPolicy) -> Result<(), String>;

    /// # Errors
    /// Returns an error when the policy is unsupported or the plan cannot be read or emitted.
    /// Implementations validate the policy, open the candidate snapshot, invoke `start` once, then
    /// invoke `emit` for its decisions. They check `scan.cancellation` between bounded scan pages.
    fn plan_retention(
        &self,
        scan: &RetentionScan<'_>,
        start: &mut dyn FnMut(peryx_policy::RetentionSummary) -> Result<(), String>,
        emit: &mut dyn FnMut(peryx_policy::RetentionDecision) -> Result<(), String>,
    ) -> Result<(), String>;
}

pub trait CacheDriver: Send + Sync {
    /// # Errors
    /// Returns an error when cached resource state cannot be read or removed.
    fn purge_resource(
        &self,
        meta: &peryx_storage::meta::MetaStore,
        index: &str,
        resource: &str,
        apply: bool,
    ) -> Result<PurgeReport, String>;
    /// # Errors
    /// Returns an error when cached pages cannot be read.
    fn cache_pages(
        &self,
        meta: &peryx_storage::meta::MetaStore,
        index_names: &[&str],
    ) -> Result<Vec<CachePage>, String>;
    /// # Errors
    /// Returns an error when cached records cannot be counted.
    fn cache_record_counts(&self, meta: &peryx_storage::meta::MetaStore) -> Result<Vec<(String, u64)>, String>;
}

/// The purge [`CacheDriver::purge_resource`] cannot do: one against a store the server is still
/// serving from.
///
/// The offline purge takes a bare [`MetaStore`](peryx_storage::meta::MetaStore) so `peryx cache purge`
/// runs with no server and no serving state at all, which also means it never meets a concurrent
/// writer - the store is exclusive, so the CLI only ever holds it when the server does not. In-process
/// the opposite is true: a refresh may be fetching the very page being removed, so an implementation
/// fences the deletion against its own cache writers and reports the counts it actually removed.
#[async_trait]
pub trait CachePurgeDriver: Send + Sync {
    /// # Errors
    /// Returns an error when cached resource state cannot be read or removed.
    async fn purge_served_resource(
        &self,
        state: Arc<ServingState>,
        index: &str,
        resource: &str,
        apply: bool,
    ) -> Result<PurgeReport, String>;
}

pub trait IndexSummaryDriver: Send + Sync {
    /// # Errors
    /// Returns an error when index state cannot be summarized.
    fn summarize_indexes(
        &self,
        meta: &peryx_storage::meta::MetaStore,
        index_names: &[String],
        recent_limit: usize,
    ) -> Result<std::collections::HashMap<String, IndexSummary>, IndexSummaryError>;
}

pub trait TrashDriver: Send + Sync {
    /// # Errors
    /// Returns an error when trash records cannot be read.
    fn trash_records(
        &self,
        meta: &peryx_storage::meta::MetaStore,
        index_names: &[String],
    ) -> Result<Vec<peryx_core::TrashRecord>, String>;
}

pub trait ImportDriver: Send + Sync {
    /// # Errors
    /// Returns an error when an input cannot be validated or stored.
    fn import_dir(
        &self,
        meta: &peryx_storage::meta::MetaStore,
        blobs: &peryx_storage::blob::BlobStorage,
        target_name: &str,
        target_route: &str,
        dir: &std::path::Path,
        out: &mut dyn Write,
    ) -> Result<(), String>;
}

#[async_trait]
pub trait ServiceDriver: Send + Sync {
    fn classify_service_post(&self, path: &str, headers: &HeaderMap) -> Option<crate::rate_limit::RouteClass>;
    async fn service_post(&self, state: Arc<ServingState>, request: Request) -> Response;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowseError {
    Denied(peryx_identity::Denial),
    /// The ecosystem's own serving gates withhold this resource - a policy or project-status
    /// decision rather than a credential question - carrying the media type and body the same
    /// ecosystem's download route answers with, so browsing cannot reach what downloading refuses.
    Refused {
        content_type: String,
        body: String,
    },
    Internal(String),
}

impl std::fmt::Display for BrowseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Denied(_) => formatter.write_str("read access denied"),
            Self::Refused { body, .. } | Self::Internal(body) => formatter.write_str(body),
        }
    }
}

impl std::error::Error for BrowseError {}

impl From<peryx_identity::Denial> for BrowseError {
    fn from(denial: peryx_identity::Denial) -> Self {
        Self::Denied(denial)
    }
}

impl From<String> for BrowseError {
    fn from(message: String) -> Self {
        Self::Internal(message)
    }
}

pub struct BrowseRequest<'a> {
    pub state: Arc<ServingState>,
    pub position: usize,
    pub raw_query: String,
    pub access: &'a crate::access::ReadAccess,
    pub base: Option<&'a crate::discovery::BaseUrl>,
}

#[async_trait]
pub trait BrowseDriver: Send + Sync {
    /// # Errors
    /// Returns a denial when the credential cannot browse the requested resource, or an error when the query cannot be
    /// resolved.
    async fn browse(&self, request: BrowseRequest<'_>) -> Result<Option<BrowsePage>, BrowseError>;
}

/// Only the credential *form* is ecosystem-specific.
///
/// An ecosystem recognizes the shape of its own index credential - a reserved Basic user id, say.
/// Resolving that credential against the index ACL is shared, so
/// [`crate::state::AppState::authorize_index_credential`] owns it for every ecosystem.
pub trait IndexCredentialDriver: Send + Sync {
    fn recognizes(&self, authorization: &str) -> bool;
}

pub trait CapabilityRegistrar {
    fn register_job(&mut self, ecosystem: Ecosystem, driver: Arc<dyn JobDriver>);
    fn register_metrics(&mut self, ecosystem: Ecosystem, driver: Arc<dyn MetricsDriver>);
    fn register_name(&mut self, ecosystem: Ecosystem, driver: Arc<dyn NameDriver>);
    fn register_policy(&mut self, ecosystem: Ecosystem, driver: Arc<dyn PolicyDriver>);
    fn register_policy_dry_run(&mut self, ecosystem: Ecosystem, driver: Arc<dyn PolicyDryRunDriver>);
    fn register_blob_references(&mut self, ecosystem: Ecosystem, driver: Arc<dyn BlobReferenceDriver>);
    fn register_fsck(&mut self, ecosystem: Ecosystem, driver: Arc<dyn FsckDriver>);
    fn register_retention(&mut self, ecosystem: Ecosystem, driver: Arc<dyn RetentionDriver>);
    fn register_cache(&mut self, ecosystem: Ecosystem, driver: Arc<dyn CacheDriver>);
    fn register_cache_purge(&mut self, ecosystem: Ecosystem, driver: Arc<dyn CachePurgeDriver>);
    fn register_index_summary(&mut self, ecosystem: Ecosystem, driver: Arc<dyn IndexSummaryDriver>);
    fn register_trash(&mut self, ecosystem: Ecosystem, driver: Arc<dyn TrashDriver>);
    fn register_import(&mut self, ecosystem: Ecosystem, driver: Arc<dyn ImportDriver>);
    fn register_service(&mut self, ecosystem: Ecosystem, driver: Arc<dyn ServiceDriver>);
    fn register_browse(&mut self, ecosystem: Ecosystem, driver: Arc<dyn BrowseDriver>);
    fn register_index_credentials(&mut self, ecosystem: Ecosystem, driver: Arc<dyn IndexCredentialDriver>);
}

pub struct CapabilityInstallContext<'a> {
    drivers: &'a mut crate::DriverSet,
    protocols: &'a mut HashMap<Ecosystem, ProtocolDriver>,
    absolute_prefixes: &'a mut Vec<(&'static str, Arc<dyn AbsoluteProtocolDriver>)>,
    rate_limit_principals: &'a mut HashMap<Ecosystem, &'static dyn RateLimitPrincipal>,
    client_discovery: &'a mut HashMap<Ecosystem, &'static dyn ClientDiscovery>,
}

impl<'a> CapabilityInstallContext<'a> {
    pub(crate) const fn new(
        drivers: &'a mut crate::DriverSet,
        protocols: &'a mut HashMap<Ecosystem, ProtocolDriver>,
        absolute_prefixes: &'a mut Vec<(&'static str, Arc<dyn AbsoluteProtocolDriver>)>,
        rate_limit_principals: &'a mut HashMap<Ecosystem, &'static dyn RateLimitPrincipal>,
        client_discovery: &'a mut HashMap<Ecosystem, &'static dyn ClientDiscovery>,
    ) -> Self {
        Self {
            drivers,
            protocols,
            absolute_prefixes,
            rate_limit_principals,
            client_discovery,
        }
    }

    pub fn replace_drivers(&mut self, drivers: crate::DriverSet) {
        *self.drivers = drivers;
    }

    pub fn register_protocol(&mut self, protocol: ProtocolDriver) {
        let ecosystem = protocol.ecosystem();
        if let Some(driver) = protocol.absolute() {
            self.absolute_prefixes
                .extend(driver.prefixes().iter().map(|&prefix| (prefix, Arc::clone(driver))));
        }
        self.protocols.insert(ecosystem, protocol);
    }

    pub fn register_rate_limit_principal(&mut self, ecosystem: Ecosystem, principal: &'static dyn RateLimitPrincipal) {
        self.rate_limit_principals.insert(ecosystem, principal);
    }

    pub fn register_client_discovery(&mut self, ecosystem: Ecosystem, discovery: &'static dyn ClientDiscovery) {
        self.client_discovery.insert(ecosystem, discovery);
    }
}

pub struct AuthInstallContext<'a> {
    serving: &'a mut ServingState,
    http_routes: &'a mut Vec<Arc<dyn HttpRoutes>>,
}

impl<'a> AuthInstallContext<'a> {
    pub(crate) const fn new(serving: &'a mut ServingState, http_routes: &'a mut Vec<Arc<dyn HttpRoutes>>) -> Self {
        Self { serving, http_routes }
    }

    #[must_use]
    pub const fn signer(&self) -> Option<&peryx_identity::Signer> {
        self.serving.signer.as_ref()
    }

    #[must_use]
    pub const fn token_ttl_secs(&self) -> i64 {
        self.serving.token_ttl_secs
    }

    #[must_use]
    pub fn writable_index_route(&self, ecosystem: &Ecosystem, name: &str) -> Option<&str> {
        self.serving
            .indexes
            .iter()
            .find(|index| {
                &index.ecosystem == ecosystem
                    && index.name == name
                    && matches!(
                        &index.kind,
                        crate::state::IndexKind::Hosted { .. }
                            | crate::state::IndexKind::Virtual {
                                write_target: Some(_),
                                ..
                            }
                    )
            })
            .map(|index| index.route.as_str())
    }

    pub fn register_service<T: Send + Sync + 'static>(&mut self, service: Arc<T>) {
        self.serving.install_plugin_service(service);
    }

    pub fn register_routes(&mut self, routes: Arc<dyn HttpRoutes>) {
        self.http_routes.push(routes);
    }
}

pub struct RuntimeInstallContext<'a> {
    serving: &'a mut ServingState,
    drivers: &'a mut crate::DriverSet,
    protocols: &'a mut HashMap<Ecosystem, ProtocolDriver>,
    absolute_prefixes: &'a mut Vec<(&'static str, Arc<dyn AbsoluteProtocolDriver>)>,
    idle_reclaimers: &'a mut HashMap<Ecosystem, Arc<dyn IdleReclaimer>>,
    intent_finalizers: &'a mut HashMap<Ecosystem, Arc<dyn IntentFinalizer>>,
    cache_refreshers: &'a mut HashMap<Ecosystem, Arc<dyn CacheRefresher>>,
    mirror_drivers: &'a mut HashMap<Ecosystem, Arc<dyn MirrorDriver>>,
    lexicons: &'a mut peryx_core::LexiconRegistry,
    http_routes: &'a mut Vec<Arc<dyn HttpRoutes>>,
}

pub(crate) struct RuntimeInstallDependencies<'a> {
    pub serving: &'a mut ServingState,
    pub drivers: &'a mut crate::DriverSet,
    pub protocols: &'a mut HashMap<Ecosystem, ProtocolDriver>,
    pub absolute_prefixes: &'a mut Vec<(&'static str, Arc<dyn AbsoluteProtocolDriver>)>,
    pub idle_reclaimers: &'a mut HashMap<Ecosystem, Arc<dyn IdleReclaimer>>,
    pub intent_finalizers: &'a mut HashMap<Ecosystem, Arc<dyn IntentFinalizer>>,
    pub cache_refreshers: &'a mut HashMap<Ecosystem, Arc<dyn CacheRefresher>>,
    pub mirror_drivers: &'a mut HashMap<Ecosystem, Arc<dyn MirrorDriver>>,
    pub lexicons: &'a mut peryx_core::LexiconRegistry,
    pub http_routes: &'a mut Vec<Arc<dyn HttpRoutes>>,
}

impl<'a> RuntimeInstallContext<'a> {
    pub(crate) const fn new(dependencies: RuntimeInstallDependencies<'a>) -> Self {
        let RuntimeInstallDependencies {
            serving,
            drivers,
            protocols,
            absolute_prefixes,
            idle_reclaimers,
            intent_finalizers,
            cache_refreshers,
            mirror_drivers,
            lexicons,
            http_routes,
        } = dependencies;
        Self {
            serving,
            drivers,
            protocols,
            absolute_prefixes,
            idle_reclaimers,
            intent_finalizers,
            cache_refreshers,
            mirror_drivers,
            lexicons,
            http_routes,
        }
    }

    #[must_use]
    pub fn has_ecosystem(&self, ecosystem: &Ecosystem) -> bool {
        self.serving.indexes.iter().any(|index| &index.ecosystem == ecosystem)
    }

    pub fn register_service<T: Send + Sync + 'static>(&mut self, service: Arc<T>) {
        self.serving.install_plugin_service(service);
    }

    pub fn register_protocol(
        &mut self,
        protocol: ProtocolDriver,
        indexer: Arc<dyn peryx_search::SearchDocumentProvider>,
    ) {
        let ecosystem = protocol.ecosystem();
        self.absolute_prefixes
            .retain(|(_, registered)| registered.ecosystem() != ecosystem);
        if let Some(driver) = protocol.absolute() {
            self.absolute_prefixes
                .extend(driver.prefixes().iter().map(|&prefix| (prefix, Arc::clone(driver))));
        }
        self.protocols.insert(ecosystem, protocol);
        self.serving.search.add_indexer(indexer);
    }

    pub fn register_browse(&mut self, ecosystem: Ecosystem, driver: Arc<dyn BrowseDriver>) {
        self.drivers.register_browse(ecosystem, driver);
    }

    pub fn register_idle_reclaimer(&mut self, ecosystem: Ecosystem, driver: Arc<dyn IdleReclaimer>) {
        self.idle_reclaimers.insert(ecosystem, driver);
    }

    pub fn register_intent_finalizer(&mut self, ecosystem: Ecosystem, driver: Arc<dyn IntentFinalizer>) {
        self.intent_finalizers.insert(ecosystem, driver);
    }

    pub fn register_cache_refresher(&mut self, ecosystem: Ecosystem, driver: Arc<dyn CacheRefresher>) {
        self.cache_refreshers.insert(ecosystem, driver);
    }

    pub fn register_mirror(&mut self, ecosystem: Ecosystem, driver: Arc<dyn MirrorDriver>) {
        self.mirror_drivers.insert(ecosystem, driver);
    }

    pub fn register_lexicon(&mut self, ecosystem: Ecosystem, lexicon: &'static peryx_core::Lexicon) {
        self.lexicons.register(ecosystem, lexicon);
    }

    pub fn register_routes(&mut self, routes: Arc<dyn HttpRoutes>) {
        self.http_routes.push(routes);
    }
}

pub struct DistributedInstallContext<'a> {
    runtime: RuntimeInstallContext<'a>,
    replicated_apply_drivers: &'a mut HashMap<Ecosystem, Arc<dyn ReplicatedApplyDriver>>,
}

impl<'a> DistributedInstallContext<'a> {
    pub(crate) const fn new(
        runtime: RuntimeInstallContext<'a>,
        replicated_apply_drivers: &'a mut HashMap<Ecosystem, Arc<dyn ReplicatedApplyDriver>>,
    ) -> Self {
        Self {
            runtime,
            replicated_apply_drivers,
        }
    }

    pub const fn runtime(&mut self) -> &mut RuntimeInstallContext<'a> {
        &mut self.runtime
    }

    pub fn register_replicated_apply(&mut self, ecosystem: Ecosystem, driver: Arc<dyn ReplicatedApplyDriver>) {
        self.replicated_apply_drivers.insert(ecosystem, driver);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MirrorAction {
    Plan,
    Sync,
    Verify,
}

pub struct MirrorRequest<'a> {
    pub action: MirrorAction,
    pub index: &'a str,
    pub settings: &'a toml::Table,
    pub configured: &'a toml::Table,
    pub overrides: &'a toml::Table,
}

#[async_trait]
pub trait MirrorDriver: Send + Sync {
    /// # Errors
    /// Returns an error when the ecosystem does not support a configured or command-line option.
    fn validate_options(&self, configured: &toml::Table, overrides: &toml::Table) -> Result<(), String>;

    async fn mirror(
        &self,
        state: Arc<crate::AppState>,
        request: MirrorRequest<'_>,
        output: &mut (dyn Write + Send),
    ) -> Result<(), String>;
}

/// The outcome of one background refresh sweep: how many cached pages a driver revalidated and how
/// many it found changed upstream.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RefreshSweep {
    pub checked: usize,
    pub changed: usize,
}

/// A resource cache purge result with driver-owned category names.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PurgeReport {
    pub resource: String,
    pub categories: Vec<(String, u64)>,
}

/// One repository's activity counts for status surfaces.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct IndexSummary {
    pub resource_count: u64,
    pub write_count: u64,
    pub recent_writes: Vec<RecentWrite>,
}

/// Status surfaces expose these classes instead of driver error text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexSummaryError {
    Storage,
    InvalidData,
}

impl IndexSummaryError {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Storage => "storage",
            Self::InvalidData => "invalid_data",
        }
    }
}

/// One recent artifact write without credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentWrite {
    pub resource: String,
    pub artifact: String,
    pub group: String,
    pub written_at: Option<String>,
    pub size: Option<u64>,
}

/// One cached repository page returned by its owning driver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachePage {
    pub index: String,
    pub resource: String,
    pub fetched_at_unix: i64,
    pub fresh_secs: Option<i64>,
    pub body_bytes: u64,
    pub record_bytes: u64,
    pub key: String,
}

pub trait EcosystemDriver: Send + Sync {
    fn ecosystem(&self) -> Ecosystem;
}

#[async_trait]
pub trait IndexedProtocolDriver: EcosystemDriver {
    fn classify_route(&self, path: &str) -> crate::rate_limit::RouteClass;
    async fn get(
        &self,
        state: Arc<ServingState>,
        position: usize,
        rest: String,
        uri: Uri,
        headers: HeaderMap,
        method: Method,
    ) -> Response;
    async fn post(&self, state: Arc<ServingState>, path: String, request: Request) -> Response;
    async fn put(&self, state: Arc<ServingState>, request: Request) -> Response;
    async fn delete(&self, state: Arc<ServingState>, request: Request) -> Response;
}

#[async_trait]
pub trait AbsoluteProtocolDriver: EcosystemDriver {
    fn prefixes(&self) -> &'static [&'static str];
    fn classify_route(&self, path: &str) -> crate::rate_limit::RouteClass;
    async fn serve(&self, state: Arc<ServingState>, request: Request) -> Response;
}

#[derive(Clone)]
pub enum ProtocolDriver {
    Indexed(Arc<dyn IndexedProtocolDriver>),
    Absolute(Arc<dyn AbsoluteProtocolDriver>),
}

impl ProtocolDriver {
    #[must_use]
    pub fn ecosystem(&self) -> Ecosystem {
        match self {
            Self::Indexed(driver) => driver.ecosystem(),
            Self::Absolute(driver) => driver.ecosystem(),
        }
    }

    #[must_use]
    pub fn driver(&self) -> &dyn EcosystemDriver {
        match self {
            Self::Indexed(driver) => driver.as_ref(),
            Self::Absolute(driver) => driver.as_ref(),
        }
    }

    #[must_use]
    pub fn driver_arc(&self) -> Arc<dyn EcosystemDriver> {
        match self {
            Self::Indexed(driver) => {
                let driver: Arc<dyn EcosystemDriver> = driver.clone();
                driver
            }
            Self::Absolute(driver) => {
                let driver: Arc<dyn EcosystemDriver> = driver.clone();
                driver
            }
        }
    }

    #[must_use]
    pub fn classify_route(&self, path: &str) -> crate::rate_limit::RouteClass {
        match self {
            Self::Indexed(driver) => driver.classify_route(path),
            Self::Absolute(driver) => driver.classify_route(path),
        }
    }

    #[must_use]
    pub fn indexed(&self) -> Option<&Arc<dyn IndexedProtocolDriver>> {
        match self {
            Self::Indexed(driver) => Some(driver),
            Self::Absolute(_) => None,
        }
    }

    #[must_use]
    pub fn absolute(&self) -> Option<&Arc<dyn AbsoluteProtocolDriver>> {
        match self {
            Self::Indexed(_) => None,
            Self::Absolute(driver) => Some(driver),
        }
    }
}
