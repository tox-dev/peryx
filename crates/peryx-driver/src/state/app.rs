use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use peryx_core::{Ecosystem, LexiconRegistry, PrometheusSource};
use peryx_storage::blob::BlobStorage;
use peryx_storage::meta::MetaStore;
use peryx_upstream::UpstreamRouter;

use peryx_index::{Index, RouteResolver};

use super::describe::{IndexDescription, describe_indexes, describe_upstream_route};
use crate::authz::AuthorizationService;
use crate::jobs::JobAttemptControl;
use crate::rate_limit::{RateLimiter, UpstreamLimits};
use crate::revocations::RevocationService;
use crate::tokens::TokenService;
use crate::users::UserService;
use peryx_events::metrics::Metrics;
use peryx_events::webhook::WebhookRuntime;
use peryx_search::SearchIndex;

pub use peryx_core::Clock;

/// Request state without driver-registry access.
pub struct ServingState {
    pub meta: MetaStore,
    /// Shared password worker bound and persistent user operations.
    pub users: UserService,
    /// Persistent server-role authorization.
    pub authorization: AuthorizationService,
    /// Digest revocation lifecycle and serving decisions.
    pub revocations: RevocationService,
    /// Scoped API token lifecycle: create, list, inspect, rotate, revoke, and verification.
    pub tokens: TokenService,
    /// Durable attempt state shared by the scheduler and management handlers.
    pub job_attempts: JobAttemptControl,
    pub blobs: BlobStorage,
    /// Cached resource pages use this fallback when upstream grants no usable freshness.
    pub ttl_secs: i64,
    /// The bound on stale-on-error serving; see [`RuntimeOptions::max_stale_secs`](crate::state::RuntimeOptions::max_stale_secs).
    pub max_stale_secs: i64,
    pub clock: Clock,
    pub requests: AtomicU64,
    /// Whether this process serves as a replica and rejects client mutations.
    pub read_only: bool,
    /// Whether this process terminates TLS itself. A response may only claim HSTS over a connection
    /// that is actually secure, and behind a reverse proxy only that proxy knows the client's scheme.
    pub tls_terminated: bool,
    pub(super) read_only_retry_after: Option<Duration>,
    pub(super) availability: AvailabilityState,
    /// Immutable repository-route positions for request dispatch.
    pub(super) route_resolver: RouteResolver,
    pub indexes: Vec<Index>,
    /// The role engine's caches for a cached (proxy) index: the single-flight map, the transformed-page
    /// cache, the negative cache, and the mutation epoch that retires them.
    pub cache: peryx_index::ServingCache,
    /// One transfer per digest prevents duplicate upstream reads.
    pub downloads: crate::download::DownloadRegistry,
    /// Off-thread usage aggregation by repository, resource, and artifact.
    pub metrics: Metrics,
    /// Derived resource search index, refreshed when the mutation epoch advances.
    pub search: SearchIndex,
    /// Reads wait for every view that can lag the committed frontier.
    pub required_views: std::sync::Arc<[&'static str]>,
    /// Per-client HTTP request limits. The bucket cache has a fixed capacity.
    pub rate_limits: RateLimiter,
    /// Per-cached-index upstream fetch gates, keyed by configured index name.
    pub upstream_limits: UpstreamLimits,
    /// Independent gates for mutable artifact metadata, so its latency cannot consume page-fetch slots.
    pub metadata_upstream_limits: UpstreamLimits,
    /// Multi-source routes keyed by cached index name. Legacy cached indexes are absent.
    pub upstream_routes: HashMap<String, UpstreamRouter>,
    /// Signed webhook delivery runtime.
    pub webhooks: WebhookRuntime,
    /// The token realm's signing key, or `None` when no signing key is configured. Without it an
    /// ecosystem's token endpoint cannot mint a JWT, so that driver can fall back to another scheme and
    /// never challenges with the Bearer scheme.
    pub signer: Option<peryx_identity::Signer>,
    /// How long a token the realm mints stays valid, in seconds.
    pub token_ttl_secs: i64,
    pub(super) plugin_services: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
    /// Named LDAP login services. Authentication routes can select one without knowing its bind mode.
    pub(super) ldap_logins: HashMap<String, Arc<peryx_identity::LdapLoginService<MetaStore>>>,
    /// Per-repository concurrency bound on retention-plan previews, so one repository's full-scan
    /// previews cannot starve the rest.
    pub retention_gates: crate::retention::RetentionGates,
    /// Named browser OIDC login services. The login and callback routes select one by provider ID.
    pub(super) oidc_logins: HashMap<String, Arc<peryx_identity::OidcLoginService<MetaStore>>>,
    /// Seals the browser session and login-handoff cookies. Present only when a token-realm signing key
    /// is configured, since the sealing key derives from it.
    pub(super) session_sealer: Option<Arc<peryx_identity::SessionSealer>>,
}

pub(super) struct AvailabilityState {
    pub(super) distributed: Option<Box<DistributedAvailability>>,
}

impl AvailabilityState {
    fn topology(&self) -> &peryx_core::TopologyConfig {
        static NONE: peryx_core::TopologyConfig = peryx_core::TopologyConfig {
            mode: peryx_core::TopologyMode::None,
            group: None,
            members: Vec::new(),
            local_node: None,
        };
        self.distributed.as_ref().map_or(&NONE, |state| &state.topology)
    }

    fn role(&self) -> peryx_core::NodeRole {
        self.distributed
            .as_ref()
            .map_or(peryx_core::NodeRole::Writer, |state| state.role)
    }

    fn analytics_completeness(&self) -> Option<&dyn peryx_ha::AnalyticsCompleteness> {
        self.distributed
            .as_ref()
            .map(|state| state.analytics_completeness.as_ref())
    }

    fn authority_drainer(&self) -> Option<&Arc<dyn peryx_ha::AuthorityDrainer>> {
        self.distributed.as_ref()?.authority_drainer.as_ref()
    }

    fn applied_frontier(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        Some(self.distributed.as_ref()?.applied_frontier.subscribe())
    }

    fn ownership_authority(&self) -> Option<&Arc<dyn peryx_ha::OwnershipAuthority>> {
        self.distributed.as_ref()?.capabilities.ownership.as_ref()
    }

    fn cross_dc_copier(&self) -> Option<&Arc<dyn peryx_ha::CrossDcCopier>> {
        self.distributed.as_ref()?.capabilities.copier.as_ref()
    }

    fn blob_reclaimer(&self) -> Option<&Arc<dyn peryx_ha::BlobReclaimer>> {
        self.distributed.as_ref()?.capabilities.reclaimer.as_ref()
    }

    fn placement_reconciler(&self) -> Option<&Arc<dyn peryx_ha::PlacementReconciler>> {
        self.distributed.as_ref()?.capabilities.placement.as_ref()
    }

    async fn ensure_blob_local(
        &self,
        digest: &peryx_storage::blob::Digest,
    ) -> Result<Option<peryx_storage::blob::BlobMetadata>, peryx_ha::BlobAvailabilityError> {
        match &self.distributed {
            Some(state) => state.ensure_blob_local(digest).await,
            None => Ok(None),
        }
    }

    async fn confirm_blob_write(&self, write: peryx_ha::CommittedBlob<'_>) -> peryx_ha::WriteDurability {
        match &self.distributed {
            Some(state) => state.confirm_blob_write(write).await,
            None => peryx_ha::WriteDurability::Confirmed {
                scope: write.evidence().scope(),
            },
        }
    }

    async fn claim_first_publish_home(
        &self,
        authority: &str,
    ) -> Result<Option<crate::state::HomeClaim>, crate::state::OwnershipError> {
        match &self.distributed {
            Some(state) => state.claim_first_publish_home(authority).await,
            None => Ok(None),
        }
    }

    async fn committed_authority_epoch(&self, authority: &str) -> u64 {
        match &self.distributed {
            Some(state) => state.committed_authority_epoch(authority).await,
            None => 0,
        }
    }

    async fn admit_authority_epoch(&self, authority: &str, presented: u64) -> bool {
        match &self.distributed {
            Some(state) => state.admit_authority_epoch(authority, presented).await,
            None => true,
        }
    }

    async fn begin_authority_epoch_write(
        &self,
        authority: &str,
        presented: u64,
    ) -> Result<Option<crate::state::AuthorityWriteLease>, crate::state::OwnershipError> {
        match &self.distributed {
            Some(state) => {
                super::ownership::begin_authority_epoch_write(
                    state.capabilities.ownership.as_ref(),
                    authority,
                    presented,
                )
                .await
            }
            None => Ok(None),
        }
    }

    async fn finish_authority_epoch_write(
        &self,
        lease: &crate::state::AuthorityWriteLease,
    ) -> Result<(), crate::state::OwnershipError> {
        match &self.distributed {
            Some(state) => {
                super::ownership::finish_authority_epoch_write(state.capabilities.ownership.as_ref(), lease).await
            }
            None => Ok(()),
        }
    }

    async fn transfer_authority_home(
        &self,
        authority: &str,
        new_home: &str,
    ) -> Result<Option<crate::state::TransferOutcome>, crate::state::OwnershipError> {
        match &self.distributed {
            Some(state) => state.transfer_authority_home(authority, new_home).await,
            None => Ok(None),
        }
    }

    fn record_home_placement(&self, digest_hex: &str, size: u64, fence: u64) {
        if let Some(state) = &self.distributed {
            state.record_home_placement(digest_hex, size, fence);
        }
    }

    pub(super) fn record_operation_trace(&self, meta: &MetaStore, kind: peryx_ha::OperationKind, fence: u64) {
        if let Some(state) = &self.distributed {
            state.record_operation_trace(meta, kind, fence);
        }
    }

    pub(super) fn publish_applied_frontier(&self, serial: u64) {
        if let Some(state) = &self.distributed {
            state.publish_applied_frontier(serial);
        }
    }
}

pub(super) struct DistributedAvailability {
    pub role: peryx_core::NodeRole,
    pub topology: peryx_core::TopologyConfig,
    pub blobs: peryx_ha::BlobServices,
    pub analytics_completeness: Arc<dyn peryx_ha::AnalyticsCompleteness>,
    pub authority_drainer: Option<Arc<dyn peryx_ha::AuthorityDrainer>>,
    pub operation_observer: Option<Arc<dyn peryx_ha::OperationObserver>>,
    pub applied_frontier: peryx_ha::AppliedFrontier,
    pub capabilities: peryx_ha::AvailabilityCapabilities,
}

impl DistributedAvailability {
    pub(super) fn new(
        role: peryx_core::NodeRole,
        topology: peryx_core::TopologyConfig,
        blobs: peryx_ha::BlobServices,
        analytics_completeness: Arc<dyn peryx_ha::AnalyticsCompleteness>,
        capabilities: peryx_ha::AvailabilityCapabilities,
        authority_drainer: Option<Arc<dyn peryx_ha::AuthorityDrainer>>,
        operation_observer: Option<Arc<dyn peryx_ha::OperationObserver>>,
    ) -> Self {
        Self {
            role,
            topology,
            blobs,
            analytics_completeness,
            authority_drainer,
            operation_observer,
            applied_frontier: peryx_ha::AppliedFrontier::default(),
            capabilities,
        }
    }
}

impl DistributedAvailability {
    async fn ensure_blob_local(
        &self,
        digest: &peryx_storage::blob::Digest,
    ) -> Result<Option<peryx_storage::blob::BlobMetadata>, peryx_ha::BlobAvailabilityError> {
        match self.blobs.availability() {
            Some(blobs) => blobs.ensure_local(digest).await,
            None => Ok(None),
        }
    }

    async fn confirm_blob_write(&self, write: peryx_ha::CommittedBlob<'_>) -> peryx_ha::WriteDurability {
        self.blobs.durability().confirm(write).await
    }

    async fn claim_first_publish_home(
        &self,
        authority: &str,
    ) -> Result<Option<crate::state::HomeClaim>, crate::state::OwnershipError> {
        crate::state::ownership::claim_first_publish_home(self.capabilities.ownership.as_ref(), authority).await
    }

    async fn committed_authority_epoch(&self, authority: &str) -> u64 {
        crate::state::ownership::committed_authority_epoch(self.capabilities.ownership.as_ref(), authority).await
    }

    async fn admit_authority_epoch(&self, authority: &str, presented: u64) -> bool {
        crate::state::ownership::admit_authority_epoch(self.capabilities.ownership.as_ref(), authority, presented).await
    }

    async fn transfer_authority_home(
        &self,
        authority: &str,
        new_home: &str,
    ) -> Result<Option<crate::state::TransferOutcome>, crate::state::OwnershipError> {
        crate::state::ownership::transfer_authority_home(self.capabilities.ownership.as_ref(), authority, new_home)
            .await
    }

    fn record_home_placement(&self, digest_hex: &str, size: u64, fence: u64) {
        let Some(recorder) = &self.capabilities.home_placement else {
            tracing::warn!(digest = digest_hex, "home placement recorder is unavailable");
            return;
        };
        if let Err(error) = recorder.record(digest_hex, size, fence) {
            tracing::warn!(
                error,
                digest = digest_hex,
                "could not record the verified home placement"
            );
        }
    }

    fn record_operation_trace(&self, meta: &MetaStore, kind: peryx_ha::OperationKind, fence: u64) {
        let Some(observer) = &self.operation_observer else {
            return;
        };
        observer.record(peryx_ha::OperationObservation {
            source: self
                .topology
                .local_node
                .clone()
                .unwrap_or_else(|| "standalone".to_owned()),
            epoch: peryx_ha::AuthorityEpoch(fence),
            serial: meta.current_serial().unwrap_or(0),
            kind,
        });
    }

    fn publish_applied_frontier(&self, serial: u64) {
        self.applied_frontier.publish(serial);
    }
}

/// Process state and installed drivers.
pub struct AppState {
    pub serving: Arc<ServingState>,
    /// Shared capacity for the request scans that run on blocking workers, held until each worker
    /// exits. It bounds HTTP request admission, so it stays out of the serving state the ecosystem
    /// drivers read.
    pub blocking_scans: crate::BlockingScanExecutor,
    pub(super) drivers: crate::DriverSet,
    /// Typed HTTP protocol drivers, one per ecosystem.
    pub(super) protocols: HashMap<Ecosystem, crate::serving::ProtocolDriver>,
    pub(super) idle_reclaimers: HashMap<Ecosystem, Arc<dyn crate::serving::IdleReclaimer>>,
    pub(super) intent_finalizers: HashMap<Ecosystem, Arc<dyn crate::serving::IntentFinalizer>>,
    pub(super) cache_refreshers: HashMap<Ecosystem, Arc<dyn crate::serving::CacheRefresher>>,
    /// Replicated-view rebuild capability implementations, installed once at startup.
    pub(super) replicated_apply_drivers: HashMap<Ecosystem, Arc<dyn crate::serving::ReplicatedApplyDriver>>,
    pub(super) mirror_drivers: HashMap<Ecosystem, Arc<dyn crate::serving::MirrorDriver>>,
    pub(super) rate_limit_principals: HashMap<Ecosystem, &'static dyn crate::serving::RateLimitPrincipal>,
    pub(super) client_discovery: HashMap<Ecosystem, &'static dyn crate::serving::ClientDiscovery>,
    pub(super) absolute_prefixes: Vec<(&'static str, Arc<dyn crate::serving::AbsoluteProtocolDriver>)>,
    /// Ecosystem vocabulary stays supplied by its driver.
    pub(super) lexicons: LexiconRegistry,
    /// Drivers supply the combined API document at startup.
    pub(super) openapi: std::sync::Arc<str>,
    pub(super) prometheus: Mutex<Vec<Arc<dyn PrometheusSource>>>,
    pub(super) http_routes: Vec<Arc<dyn crate::HttpRoutes>>,
}

impl AppState {
    pub fn register_prometheus(&self, source: Arc<dyn PrometheusSource>) {
        self.prometheus
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(source);
    }

    pub fn write_process_metrics(&self, body: &mut String) {
        for source in self
            .prometheus
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
        {
            source.write_metrics(body);
        }
    }

    pub fn http_routes(&self) -> impl Iterator<Item = &Arc<dyn crate::HttpRoutes>> {
        self.http_routes.iter()
    }
}

impl ServingState {
    /// How long a configured replica waits between synchronization attempts.
    #[must_use]
    pub const fn read_only_retry_after(&self) -> Option<Duration> {
        self.read_only_retry_after
    }

    #[must_use]
    pub fn plugin_service<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.plugin_services.get(&TypeId::of::<T>())?.downcast_ref()
    }

    #[must_use]
    pub async fn is_ready(&self, writes: bool) -> bool {
        self.meta.current_serial().is_ok() && self.blobs.health().await.is_ok() && (!writes || !self.read_only)
    }

    #[must_use]
    pub fn availability_topology(&self) -> &peryx_core::TopologyConfig {
        self.availability.topology()
    }

    /// # Errors
    /// Returns the configured availability provider's lookup or transfer failure.
    pub async fn ensure_blob_local(
        &self,
        digest: &peryx_storage::blob::Digest,
    ) -> Result<Option<peryx_storage::blob::BlobMetadata>, peryx_ha::BlobAvailabilityError> {
        self.availability.ensure_blob_local(digest).await
    }

    pub async fn confirm_blob_write(&self, write: peryx_ha::CommittedBlob<'_>) -> peryx_ha::WriteDurability {
        self.availability.confirm_blob_write(write).await
    }

    #[must_use]
    pub fn analytics_completeness(&self) -> Option<&dyn peryx_ha::AnalyticsCompleteness> {
        self.availability.analytics_completeness()
    }

    #[must_use]
    pub fn authority_drainer(&self) -> Option<&Arc<dyn peryx_ha::AuthorityDrainer>> {
        self.availability.authority_drainer()
    }

    #[must_use]
    pub fn replica_applied_frontier(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        self.availability.applied_frontier()
    }

    #[must_use]
    pub fn availability_role(&self) -> peryx_core::NodeRole {
        self.availability.role()
    }

    #[must_use]
    pub fn ownership_authority(&self) -> Option<&Arc<dyn peryx_ha::OwnershipAuthority>> {
        self.availability.ownership_authority()
    }

    #[must_use]
    pub fn cross_dc_copier(&self) -> Option<&Arc<dyn peryx_ha::CrossDcCopier>> {
        self.availability.cross_dc_copier()
    }

    #[must_use]
    pub fn blob_reclaimer(&self) -> Option<&Arc<dyn peryx_ha::BlobReclaimer>> {
        self.availability.blob_reclaimer()
    }

    #[must_use]
    pub fn placement_reconciler(&self) -> Option<&Arc<dyn peryx_ha::PlacementReconciler>> {
        self.availability.placement_reconciler()
    }

    /// Placement failure cannot invalidate an already committed blob.
    pub fn record_home_placement(&self, digest_hex: &str, size: u64, fence: u64) {
        self.availability.record_home_placement(digest_hex, size, fence);
    }

    /// Returns `authority`'s committed home and epoch, assigning the local datacenter when unowned.
    /// `None` means this process runs without distributed ownership.
    ///
    /// # Errors
    /// Returns the ownership group's resolution or commit error.
    pub async fn claim_first_publish_home(
        &self,
        authority: &str,
    ) -> Result<Option<crate::state::HomeClaim>, crate::state::OwnershipError> {
        self.availability.claim_first_publish_home(authority).await
    }

    /// The committed authority epoch for `authority`, the fence value a writer stamps onto work it
    /// produces so a stale-epoch write is fenced out. `0` when this process runs no consensus group,
    /// which the placement fence reads as the closed, unassigned sentinel.
    pub async fn committed_authority_epoch(&self, authority: &str) -> u64 {
        self.availability.committed_authority_epoch(authority).await
    }

    /// Whether background work carrying `presented` under `authority` may still be written, or is fenced
    /// as a stale-epoch writer that the authority superseded. A process running no consensus group has no
    /// authority to supersede its work, so it admits everything.
    pub async fn admit_authority_epoch(&self, authority: &str, presented: u64) -> bool {
        self.availability.admit_authority_epoch(authority, presented).await
    }

    /// Acquire the quorum lease that spans one metadata commit, or `None` without distributed ownership.
    ///
    /// # Errors
    /// Returns the ownership error when the lease cannot commit.
    pub async fn begin_authority_epoch_write(
        &self,
        authority: &str,
        presented: u64,
    ) -> Result<Option<crate::state::AuthorityWriteLease>, crate::state::OwnershipError> {
        self.availability
            .begin_authority_epoch_write(authority, presented)
            .await
    }

    /// # Errors
    /// Returns the ownership error when the quorum cannot release the lease.
    pub async fn finish_authority_epoch_write(
        &self,
        lease: &crate::state::AuthorityWriteLease,
    ) -> Result<(), crate::state::OwnershipError> {
        self.availability.finish_authority_epoch_write(lease).await
    }

    /// # Errors
    /// The [`OwnershipError`](crate::state::OwnershipError) the commit failed with.
    pub async fn transfer_authority_home(
        &self,
        authority: &str,
        new_home: &str,
    ) -> Result<Option<crate::state::TransferOutcome>, crate::state::OwnershipError> {
        self.availability.transfer_authority_home(authority, new_home).await
    }

    #[must_use]
    pub fn resolve<'a>(&'a self, path: &'a str) -> Option<(&'a Index, &'a str)> {
        self.resolve_position(path)
            .map(|(position, rest)| (&self.indexes[position], rest))
    }

    /// Like [`Self::resolve`], returning the index position instead of a borrow.
    #[must_use]
    pub fn resolve_position<'a>(&self, path: &'a str) -> Option<(usize, &'a str)> {
        self.route_resolver.resolve(path)
    }

    #[must_use]
    pub fn index_at(&self, pos: usize) -> &Index {
        &self.indexes[pos]
    }

    #[must_use]
    pub fn describe_indexes(&self) -> Vec<IndexDescription> {
        let mut descriptions = describe_indexes(&self.indexes);
        for description in &mut descriptions {
            if let (Some(router), Some(upstream)) = (
                self.upstream_routes.get(&description.name),
                description.upstream.as_mut(),
            ) {
                (upstream.status, upstream.sources) = describe_upstream_route(router);
            }
        }
        descriptions
    }

    #[must_use]
    pub fn ldap_login(&self, provider: &str) -> Option<&peryx_identity::LdapLoginService<MetaStore>> {
        self.ldap_logins.get(provider).map(AsRef::as_ref)
    }

    #[must_use]
    pub fn oidc_login(&self, provider: &str) -> Option<&peryx_identity::OidcLoginService<MetaStore>> {
        self.oidc_logins.get(provider).map(AsRef::as_ref)
    }

    #[must_use]
    pub fn oidc_providers(&self) -> Vec<&str> {
        let mut providers = self.oidc_logins.keys().map(String::as_str).collect::<Vec<_>>();
        providers.sort_unstable();
        providers
    }

    #[must_use]
    pub fn session_sealer(&self) -> Option<&peryx_identity::SessionSealer> {
        self.session_sealer.as_deref()
    }
}

/// Signed webhook delivery borrows exactly three things from the process - the configured targets,
/// the queue's store, and the clock - and reaches them through this trait rather than the whole state.
impl peryx_events::webhook::WebhookHost for ServingState {
    fn webhooks(&self) -> &WebhookRuntime {
        &self.webhooks
    }

    fn meta(&self) -> &MetaStore {
        &self.meta
    }

    fn now(&self) -> i64 {
        (self.clock)()
    }
}

#[cfg(test)]
#[path = "../../tests/unit/state/app/tests.rs"]
mod tests;
