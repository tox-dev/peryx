use std::any::Any;
use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Context as _;
use axum::Router;
use futures_util::FutureExt as _;
use peryx_core::{Clock, NodeRole, PrometheusSource, TopologyConfig, TopologyMember, TopologyMode};
use peryx_driver::{AppState, DriverSet};
use peryx_ha::{
    AnalyticsCompleteness, AvailabilityAssembler, AvailabilityCapabilities, AvailabilityFailure, AvailabilityInstall,
    AvailabilityShutdownError, AvailabilityShutdownStage, AvailabilityTaskError, AvailabilityTaskReport, BlobServices,
    ControlAuthorizer, ControlExecutor, OwnershipAuthority, ReceiptSource, ReclamationFrontiers, ReferenceInventory,
    RemoteFrontierSource,
};
use peryx_storage::blob::{BlobStorage, BlobStore};
use peryx_storage::meta::{BackendId, DataCenterId, MetaStore};

use crate::control_http::{AvailabilityPosture, AvailabilityPostureRole, ControlHttpContext, availability_router};
use crate::lifecycle::{FailureReceiver, Lifecycle};
use crate::{
    BlobReclamationSelector, CrossDcBlobCopier, DcDurabilityMetrics, DistributedAnalyticsCompleteness,
    DistributedBlobDurability, DistributedMode, FilesystemPlacementReconciler, HttpReceiptSource,
    HttpRemoteFrontierSource, RosterFrontierSource, RuntimeConfig, RuntimeMemberRole, RuntimeMembership, RuntimeRole,
    TransferCoordinator, recover_transfer_audits, remote_blob_availability,
};

const RECEIPT_FETCH_TIMEOUT: Duration = Duration::from_secs(5);

pub struct DistributedServiceConfig {
    pub runtime: RuntimeConfig,
    pub read_only: bool,
    pub write_ack_deadline: Duration,
}

#[derive(Clone)]
pub struct DistributedServiceContext {
    pub meta: peryx_storage::meta::MetaStore,
    pub blobs: BlobStorage,
    pub clock: Clock,
}

pub struct DistributedServiceAssembly;

/// # Errors
/// Returns a storage or service assembly failure.
pub fn install_services(config: &DistributedServiceConfig, state: &mut AppState) -> anyhow::Result<()> {
    state.serving.meta.initialize_distributed_state()?;
    let mut services = <DistributedServiceAssembly as AvailabilityAssembler>::assemble(
        config,
        &DistributedServiceContext {
            meta: state.serving.meta.clone(),
            blobs: state.serving.blobs.clone(),
            clock: state.serving.clock.clone(),
        },
    )?;
    let bindings = RuntimeBindings::new(config.runtime.mode == DistributedMode::Ha);
    services.capabilities = assemble_workers(
        &config.runtime,
        DistributedWorkerContext {
            filesystem: state.serving.blobs.filesystem_store().cloned(),
            backend: state.serving.blobs.backend_id(),
            meta: state.serving.meta.clone(),
            blobs: state.serving.blobs.clone(),
            clock: state.serving.clock.clone(),
            authority: bindings.authority(),
            references: bindings.references.clone(),
            frontiers: bindings.frontiers.clone(),
        },
    )?;
    bindings.gate(&mut services.capabilities);
    state
        .register_plugin_service(Arc::new(bindings))
        .map_err(anyhow::Error::msg)
        .context("failed to register distributed runtime bindings")?;
    state
        .install_distributed_availability(peryx_ha::AvailabilityStateInstall {
            role: services.role,
            topology: services.topology,
            blobs: services.blobs,
            analytics: services.analytics,
            capabilities: services.capabilities,
            authority_drainer: services.authority_drainer,
            operations: Some(services.operations),
        })
        .map_err(anyhow::Error::msg)
        .context("failed to install distributed availability")?;
    state.register_http_routes(services.routes);
    for metrics in services.metrics {
        state.register_prometheus(metrics);
    }
    Ok(())
}

impl AvailabilityAssembler for DistributedServiceAssembly {
    type Config = DistributedServiceConfig;
    type Context = DistributedServiceContext;
    type Routes = Arc<dyn peryx_driver::HttpRoutes>;
    type Error = anyhow::Error;

    fn assemble(config: &Self::Config, context: &Self::Context) -> anyhow::Result<AvailabilityInstall<Self::Routes>> {
        let topology = topology(&config.runtime, config.read_only);
        let metrics = Arc::new(DcDurabilityMetrics::default());
        let durability = Arc::new(DistributedBlobDurability::new(
            topology.clone(),
            config.runtime.write_ack_policy,
            receipt_sources(&config.runtime)?,
            remote_frontier_sources(&config.runtime)?,
            config.write_ack_deadline,
            metrics.clone(),
        ));
        let analytics: Arc<dyn AnalyticsCompleteness> = Arc::new(DistributedAnalyticsCompleteness);
        let prometheus: Arc<dyn PrometheusSource> = metrics;
        Ok(AvailabilityInstall {
            role: runtime_role(&config.runtime.role),
            topology,
            blobs: BlobServices::new(
                remote_blob_availability(
                    &config.runtime,
                    context.meta.clone(),
                    context.blobs.clone(),
                    context.clock.clone(),
                )?,
                durability,
            ),
            analytics,
            operations: Arc::new(crate::telemetry::DistributedOperationObserver),
            capabilities: AvailabilityCapabilities::default(),
            authority_drainer: Some(Arc::new(crate::DistributedAuthorityDrainer::new(context.meta.clone()))),
            metrics: vec![prometheus],
            routes: Arc::new(crate::DistributedHttpRoutes),
        })
    }
}

pub struct AvailabilityControlContext {
    pub authorizer: Arc<dyn ControlAuthorizer>,
    pub read_only: bool,
    pub meta: peryx_storage::meta::MetaStore,
    pub control: Option<Arc<dyn ControlExecutor>>,
    pub ownership: Option<Arc<dyn OwnershipAuthority>>,
}

pub struct DistributedWorkerContext {
    pub filesystem: Option<BlobStore>,
    pub backend: BackendId,
    pub meta: peryx_storage::meta::MetaStore,
    pub blobs: BlobStorage,
    pub clock: Clock,
    pub authority: Option<Arc<dyn OwnershipAuthority>>,
    pub references: Arc<dyn ReferenceInventory>,
    pub frontiers: Arc<dyn ReclamationFrontiers>,
}

pub struct DistributedPrepareContext {
    pub config: RuntimeConfig,
    pub state: Arc<AppState>,
    pub control_authorizer: Arc<dyn ControlAuthorizer>,
    pub references: Arc<dyn ReferenceInventory>,
    pub listener: Option<Box<dyn PreparedAvailabilityListener>>,
}

pub type AvailabilityListenerFuture = Pin<Box<dyn Future<Output = Result<(), AvailabilityListenerError>> + Send>>;

#[derive(Clone, Debug, thiserror::Error)]
pub enum AvailabilityListenerError {
    #[error("availability listener setup failed: {0}")]
    Setup(#[source] Arc<std::io::Error>),
    #[error("availability listener failed: {0}")]
    Serve(#[source] Arc<std::io::Error>),
    #[error("availability listener stopped unexpectedly")]
    Stopped,
    #[error("availability listener task failed: {0}")]
    Task(String),
}

impl AvailabilityListenerError {
    #[must_use]
    pub fn setup(error: std::io::Error) -> Self {
        Self::Setup(Arc::new(error))
    }

    #[must_use]
    pub fn serve(error: std::io::Error) -> Self {
        Self::Serve(Arc::new(error))
    }
}

impl From<std::io::Error> for AvailabilityListenerError {
    fn from(error: std::io::Error) -> Self {
        Self::setup(error)
    }
}

pub trait PreparedAvailabilityListener: Send {
    fn address(&self) -> SocketAddr;

    /// # Errors
    /// Returns a setup failure before the listener task starts.
    fn serve(
        self: Box<Self>,
        router: Router,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Result<AvailabilityListenerFuture, AvailabilityListenerError>;
}

struct RunningListener {
    thread: ListenerThread,
    lifecycle: Lifecycle,
}

enum ListenerThread {
    Running(std::thread::JoinHandle<Result<(), AvailabilityListenerError>>),
    Joined,
}

impl RunningListener {
    async fn start(
        listener: Box<dyn PreparedAvailabilityListener>,
        router: Router,
        lifecycle: Lifecycle,
    ) -> Result<Self, AvailabilityListenerError> {
        let (setup, setup_result) = tokio::sync::oneshot::channel();
        let thread_shutdown = lifecycle.cancellation();
        let supervision = lifecycle.clone();
        let task_supervision = lifecycle.clone();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        let thread = std::thread::Builder::new()
            .name("peryx-availability-listener".to_owned())
            .spawn(move || {
                let future = {
                    let _guard = runtime.enter();
                    listener.serve(router, thread_shutdown.clone())
                };
                let future = match future {
                    Ok(future) => future,
                    Err(error) => {
                        let _ = setup.send(Err(error.clone()));
                        return Err(error);
                    }
                };
                if setup.send(Ok(())).is_err() {
                    thread_shutdown.cancel();
                    return Err(AvailabilityListenerError::Task(
                        "listener startup was cancelled".to_owned(),
                    ));
                }
                let result = runtime.block_on(async move {
                    if !task_supervision.activated().await {
                        return Ok(());
                    }
                    match std::panic::AssertUnwindSafe(future).catch_unwind().await {
                        Ok(Ok(())) if thread_shutdown.is_cancelled() => Ok(()),
                        Ok(Ok(())) => Err(AvailabilityListenerError::Stopped),
                        Ok(Err(error)) => Err(error),
                        Err(payload) => Err(AvailabilityListenerError::Task(panic_message(payload.as_ref()))),
                    }
                });
                if let Err(error) = &result {
                    supervision.fail(error.to_string());
                }
                result
            })
            .map_err(listener_thread_error)?;
        match setup_result.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                let _ = join_listener_thread(thread);
                return Err(error);
            }
            Err(_) => {
                return Err(join_listener_thread(thread).expect_err("setup sender only drops when its thread panics"));
            }
        }
        Ok(Self {
            thread: ListenerThread::Running(thread),
            lifecycle,
        })
    }

    fn cancel(&self) {
        self.lifecycle.cancel();
    }

    fn join(&mut self) -> Result<(), AvailabilityListenerError> {
        match std::mem::replace(&mut self.thread, ListenerThread::Joined) {
            ListenerThread::Running(thread) => join_listener_thread(thread),
            ListenerThread::Joined => Ok(()),
        }
    }
}

fn join_listener_thread(
    thread: std::thread::JoinHandle<Result<(), AvailabilityListenerError>>,
) -> Result<(), AvailabilityListenerError> {
    thread
        .join()
        .map_err(|payload| AvailabilityListenerError::Task(panic_message(payload.as_ref())))?
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_owned();
    }
    payload
        .downcast_ref::<String>()
        .cloned()
        .unwrap_or_else(|| "panic".to_owned())
}

fn listener_thread_error(error: std::io::Error) -> AvailabilityListenerError {
    let message = error.to_string();
    drop(error);
    AvailabilityListenerError::Task(message)
}

impl Drop for RunningListener {
    fn drop(&mut self) {
        self.lifecycle.cancel();
        if let ListenerThread::Running(thread) = std::mem::replace(&mut self.thread, ListenerThread::Joined) {
            reap_thread("peryx-availability-listener-reaper", thread);
        }
    }
}

struct ActiveDistributed {
    lifecycle: Lifecycle,
    listener: OwnedResource<RunningListener>,
    consensus: OwnedResource<crate::Consensus>,
    runtime: OwnedResource<crate::AvailabilityRuntime>,
    bindings: RuntimeBindings,
}

struct PreparedDistributed {
    runtime: crate::runtime::PreparedDistributedRuntime,
    context: DistributedPrepareContext,
    lifecycle: Lifecycle,
    bindings: RuntimeBindings,
}

pub struct DistributedHandle {
    prepared: PreparedDistributed,
    failures: FailureReceiver,
}

pub struct DistributedActiveHandle {
    active: ActiveDistributed,
    failures: FailureReceiver,
}

enum OwnedResource<T> {
    Absent,
    Owned(T),
    Joining(ShutdownOwner),
    Complete,
}

impl<T> OwnedResource<T> {
    const fn as_ref(&self) -> Option<&T> {
        match self {
            Self::Owned(owner) => Some(owner),
            Self::Absent | Self::Joining(_) | Self::Complete => None,
        }
    }

    const fn as_mut(&mut self) -> Option<&mut T> {
        match self {
            Self::Owned(owner) => Some(owner),
            Self::Absent | Self::Joining(_) | Self::Complete => None,
        }
    }
}

impl<T> From<Option<T>> for OwnedResource<T> {
    fn from(owner: Option<T>) -> Self {
        owner.map_or(Self::Absent, Self::Owned)
    }
}

pub fn reap_process_resource<E>(
    name: &'static str,
    reap: impl FnOnce() -> Result<(), E> + Send + 'static,
) -> std::thread::JoinHandle<()>
where
    E: std::fmt::Display + Send + 'static,
{
    reap_process_resource_observed(name, reap, || {})
}

fn reap_process_resource_observed<E>(
    name: &'static str,
    reap: impl FnOnce() -> Result<(), E> + Send + 'static,
    completed: impl FnOnce() + Send + 'static,
) -> std::thread::JoinHandle<()>
where
    E: std::fmt::Display + Send + 'static,
{
    std::thread::Builder::new()
        .name("peryx-resource-reaper".to_owned())
        .spawn(move || {
            run_reaper_job(name, reap);
            completed();
        })
        .expect("spawn process resource reaper")
}

fn run_reaper_job<E>(name: &'static str, reap: impl FnOnce() -> Result<(), E>)
where
    E: std::fmt::Display,
{
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match reap() {
        Ok(()) => tracing::debug!(resource = name, "resource shutdown completed"),
        Err(error) => tracing::error!(resource = name, %error, "resource shutdown failed"),
    }))
    .is_err()
    {
        tracing::error!(resource = name, "resource reaper panicked");
    }
}

type ShutdownWorker = (
    tokio::sync::oneshot::Receiver<Result<(), std::io::Error>>,
    std::thread::JoinHandle<()>,
);

struct ShutdownOwner {
    stage: AvailabilityShutdownStage,
    worker: Option<ShutdownWorker>,
    completion_sender: tokio::sync::watch::Sender<bool>,
}

struct ShutdownFailure {
    stage: AvailabilityShutdownStage,
    error: std::io::Error,
    completion: Option<tokio::sync::watch::Receiver<bool>>,
}

impl ShutdownOwner {
    fn start<T, F, E>(stage: AvailabilityShutdownStage, owner: T, shutdown: F) -> Self
    where
        T: Send + 'static,
        F: FnOnce(T) -> Result<(), E> + Send + 'static,
        E: std::fmt::Display + Send + 'static,
    {
        let (completed, result) = tokio::sync::oneshot::channel();
        let completion_sender = tokio::sync::watch::channel(false).0;
        let thread = std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| shutdown(owner)))
                .map_err(|payload| {
                    std::io::Error::other(format!("shutdown panicked: {}", panic_message(payload.as_ref())))
                })
                .and_then(|result| result.map_err(|error| std::io::Error::other(error.to_string())));
            let _ = completed.send(result);
        });
        Self {
            stage,
            worker: Some((result, thread)),
            completion_sender,
        }
    }

    async fn wait(&mut self, deadline: Duration) -> Option<ShutdownFailure> {
        let (mut result, thread) = self.worker.take()?;
        let completed = tokio::time::timeout(deadline, &mut result).await;
        let Ok(result) = completed else {
            self.worker = Some((result, thread));
            return Some(ShutdownFailure {
                stage: self.stage,
                error: std::io::Error::other("shutdown deadline exceeded"),
                completion: Some(self.completion_sender.subscribe()),
            });
        };
        thread.join().expect("shutdown owner catches task panics");
        self.completion_sender.send_replace(true);
        match result.expect("shutdown owner always reports") {
            Ok(()) => None,
            Err(error) => Some(ShutdownFailure {
                stage: self.stage,
                error,
                completion: None,
            }),
        }
    }
}

impl Drop for ShutdownOwner {
    fn drop(&mut self) {
        if let Some((result, thread)) = self.worker.take() {
            let completion_sender = self.completion_sender.clone();
            drop(reap_process_resource_observed(
                "availability shutdown",
                move || {
                    let result = result.blocking_recv().expect("shutdown owner always reports");
                    thread.join().expect("shutdown owner catches task panics");
                    result
                },
                move || {
                    completion_sender.send_replace(true);
                },
            ));
        }
    }
}

impl<T: Send + 'static> OwnedResource<T> {
    async fn shutdown<F, E>(
        &mut self,
        stage: AvailabilityShutdownStage,
        deadline: Duration,
        shutdown: F,
    ) -> Option<ShutdownFailure>
    where
        F: FnOnce(T) -> Result<(), E> + Send + 'static,
        E: std::fmt::Display + Send + 'static,
    {
        let resource = match std::mem::replace(self, Self::Complete) {
            Self::Owned(owner) => Self::Joining(ShutdownOwner::start(stage, owner, shutdown)),
            resource => resource,
        };
        *self = resource;
        self.wait_shutdown(deadline).await
    }

    async fn wait_shutdown(&mut self, deadline: Duration) -> Option<ShutdownFailure> {
        let resource = std::mem::replace(self, Self::Complete);
        let Self::Joining(mut owner) = resource else {
            *self = resource;
            return None;
        };
        let failure = owner.wait(deadline).await;
        if failure.as_ref().is_some_and(|failure| failure.completion.is_some()) {
            *self = Self::Joining(owner);
        }
        failure
    }
}

fn record_shutdown_failure(
    failure: &mut Option<AvailabilityShutdownError>,
    stage: AvailabilityShutdownStage,
    error: impl std::error::Error + Send + Sync + 'static,
) {
    match failure {
        Some(failure) => failure.push(stage, error),
        None => *failure = Some(AvailabilityShutdownError::new(stage, error)),
    }
}

impl DistributedHandle {
    #[must_use]
    pub fn listener_address(&self) -> Option<std::net::SocketAddr> {
        self.prepared
            .context
            .listener
            .as_ref()
            .map(|listener| listener.address())
    }
}

impl ActiveDistributed {
    async fn shutdown_owned(&mut self) -> Result<(), AvailabilityShutdownError> {
        self.bindings.deactivate();
        self.lifecycle.cancel();
        if let Some(listener) = self.listener.as_ref() {
            listener.cancel();
        }
        if let Some(runtime) = self.runtime.as_mut() {
            runtime.cancel_workers();
        }
        if let Some(consensus) = self.consensus.as_ref() {
            consensus.cancel();
        }
        let listener = self.listener.shutdown(
            AvailabilityShutdownStage::Listener,
            SHUTDOWN_DEADLINE,
            |mut listener| listener.join(),
        );
        let consensus = self.consensus.shutdown(
            AvailabilityShutdownStage::Consensus,
            SHUTDOWN_DEADLINE,
            crate::Consensus::shutdown,
        );
        let runtime = self.runtime.shutdown(
            AvailabilityShutdownStage::Runtime,
            SHUTDOWN_DEADLINE,
            crate::AvailabilityRuntime::terminate_workers,
        );
        let (listener, consensus, runtime) = tokio::join!(listener, consensus, runtime);
        let mut failure = None;
        for result in <[_; 3]>::from((listener, consensus, runtime)).into_iter().flatten() {
            record_shutdown_failure(&mut failure, result.stage, result.error);
        }
        failure.map_or(Ok(()), Err)
    }
}

impl Drop for ActiveDistributed {
    fn drop(&mut self) {
        self.bindings.deactivate();
        self.lifecycle.cancel();
        if let Some(listener) = self.listener.as_ref() {
            listener.cancel();
        }
        if let Some(runtime) = self.runtime.as_mut() {
            runtime.cancel_workers();
        }
        if let Some(consensus) = self.consensus.as_ref() {
            consensus.cancel();
        }
    }
}

const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(10);

fn reap_thread(name: &'static str, thread: std::thread::JoinHandle<Result<(), AvailabilityListenerError>>) {
    drop(reap_process_resource(name, move || join_listener_thread(thread)));
}

#[async_trait::async_trait]
impl peryx_ha::AvailabilityHandle for DistributedHandle {
    type Active = DistributedActiveHandle;
    type Error = anyhow::Error;

    fn activate(self) -> anyhow::Result<Self::Active> {
        let Self { prepared, failures } = self;
        let active = std::thread::Builder::new()
            .name("peryx-availability-startup".to_owned())
            .spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .context("build the availability startup runtime")?
                    .block_on(activate_runtime(prepared))
            })
            .context("spawn the availability startup thread")?
            .join()
            .expect("availability startup reports subsystem errors")?;
        Ok(DistributedActiveHandle { active, failures })
    }

    async fn shutdown(self) -> Result<(), AvailabilityShutdownError> {
        self.prepared.bindings.deactivate();
        self.prepared.lifecycle.cancel();
        self.prepared
            .runtime
            .shutdown()
            .await
            .expect("prepared runtime owner exits after cancellation");
        Ok(())
    }
}

#[async_trait::async_trait]
impl peryx_ha::ActiveAvailabilityHandle for DistributedActiveHandle {
    async fn wait_for_failure(&mut self) -> AvailabilityFailure {
        AvailabilityFailure::new(self.failures.wait().await)
    }

    async fn shutdown(&mut self) -> Result<(), AvailabilityShutdownError> {
        self.active.shutdown_owned().await
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{startup}; cleanup failed: {cleanup}")]
struct StartupCleanupError {
    startup: anyhow::Error,
    cleanup: AvailabilityShutdownError,
}

async fn fail_startup<T>(startup: anyhow::Error, mut active: ActiveDistributed) -> anyhow::Result<T> {
    if let Err(cleanup) = active.shutdown_owned().await {
        return Err(StartupCleanupError { startup, cleanup }.into());
    }
    Err(startup)
}

async fn recover_startup_transfer_audits(outbox: &dyn crate::TransferAuditOutbox, meta: &MetaStore) {
    if let Err(error) = recover_transfer_audits(outbox, meta).await {
        tracing::warn!(%error, "transfer audit recovery at startup failed");
    }
}

pub fn prepare_runtime(
    runtime: crate::DistributedRuntime,
    context: DistributedPrepareContext,
) -> anyhow::Result<peryx_ha::PreparedAvailability<Router, DistributedHandle>> {
    let (lifecycle, failures) = Lifecycle::new();
    let public_routes = peryx_ha::AvailabilityRuntime::routes(&runtime);
    let metrics = peryx_ha::AvailabilityRuntime::metrics(&runtime);
    let is_replica = runtime.is_replica();
    anyhow::ensure!(
        !runtime.requires_control_listener() || context.listener.is_some(),
        "HA runtime requires a bound availability listener"
    );
    let frontiers = runtime.reclamation_frontiers();
    let bindings = context
        .state
        .serving
        .plugin_service::<RuntimeBindings>()
        .cloned()
        .context("distributed capability bindings are not installed")?;
    bindings.references.bind(context.references.clone());
    bindings.frontiers.bind(frontiers);
    let runtime = runtime
        .prepare_worker_runtime()
        .context("start the availability worker runtime")?;
    Ok(peryx_ha::PreparedAvailability {
        public_routes,
        metrics,
        is_replica,
        handle: DistributedHandle {
            prepared: PreparedDistributed {
                runtime,
                context,
                lifecycle,
                bindings,
            },
            failures,
        },
    })
}

async fn activate_runtime(mut prepared: PreparedDistributed) -> anyhow::Result<ActiveDistributed> {
    let mut active = ActiveDistributed {
        consensus: prepared
            .runtime
            .ignite_consensus_with_lifecycle(prepared.lifecycle.clone())
            .await?
            .into(),
        lifecycle: prepared.lifecycle.clone(),
        listener: OwnedResource::Absent,
        runtime: OwnedResource::Absent,
        bindings: prepared.bindings.clone(),
    };
    let consensus = active.consensus.as_ref();
    let ownership = consensus.map(|value| value.authority.clone());
    if let Some(ownership) = ownership.as_ref() {
        recover_startup_transfer_audits(ownership, &prepared.context.state.serving.meta).await;
    }
    let control_routes = availability_control_router(
        &prepared.context.config,
        AvailabilityControlContext {
            authorizer: prepared.context.control_authorizer,
            read_only: prepared.context.state.serving.read_only,
            meta: prepared.context.state.serving.meta.clone(),
            control: consensus.map(|value| value.control.clone()),
            ownership: ownership.clone(),
        },
    );
    active.runtime = prepared
        .runtime
        .start_with_lifecycle(prepared.lifecycle.clone())
        .expect("prepared runtime reserves its configured worker slots")
        .into();
    if let Some(listener) = prepared.context.listener.take() {
        active.listener = match RunningListener::start(listener, control_routes, prepared.lifecycle.clone()).await {
            Ok(listener) => OwnedResource::Owned(listener),
            Err(error) => return fail_startup(error.into(), active).await,
        };
    }
    prepared.bindings.bind_ownership(ownership);
    prepared.lifecycle.activate();
    prepared.bindings.activate();
    Ok(active)
}

struct DriverReferences {
    meta: peryx_storage::meta::MetaStore,
    drivers: DriverSet,
    index_names: Vec<String>,
}

impl ReferenceInventory for DriverReferences {
    fn referenced(&self) -> Result<BTreeSet<String>, String> {
        let mut referenced = self
            .drivers
            .scan_blob_references(&self.meta)
            .map_err(|error| error.to_string())?
            .digests;
        for (ecosystem, trash) in self.drivers.trash_drivers() {
            for record in trash
                .trash_records(&self.meta, &self.index_names)
                .map_err(|reason| format!("scan {} trash: {reason}", ecosystem.as_str()))?
            {
                referenced.extend(
                    record
                        .digest
                        .map(|digest| digest.strip_prefix("sha256:").unwrap_or(&digest).to_owned()),
                );
            }
        }
        Ok(referenced)
    }
}

#[must_use]
pub fn reference_inventory(
    drivers: DriverSet,
    meta: peryx_storage::meta::MetaStore,
    index_names: Vec<String>,
) -> Arc<dyn ReferenceInventory> {
    Arc::new(DriverReferences {
        meta,
        drivers,
        index_names,
    })
}

struct BoundCrossDcBlobCopier {
    copier: CrossDcBlobCopier,
    meta: peryx_storage::meta::MetaStore,
    clock: Clock,
    authority: Option<Arc<dyn OwnershipAuthority>>,
}

struct GatedCrossDcCopier {
    copier: Arc<dyn peryx_ha::CrossDcCopier>,
    active: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl peryx_ha::CrossDcCopier for GatedCrossDcCopier {
    async fn copy_pass(
        &self,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
        concurrency: std::num::NonZeroUsize,
    ) -> Result<AvailabilityTaskReport, AvailabilityTaskError> {
        ensure_active(&self.active)?;
        self.copier.copy_pass(cancelled, concurrency).await
    }
}

#[async_trait::async_trait]
impl peryx_ha::CrossDcCopier for BoundCrossDcBlobCopier {
    async fn copy_pass(
        &self,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
        concurrency: std::num::NonZeroUsize,
    ) -> Result<AvailabilityTaskReport, AvailabilityTaskError> {
        self.copier
            .copy_pass(
                &self.meta,
                &self.clock,
                self.authority
                    .as_ref()
                    .map_or(0, |authority| authority.cluster_status().term),
                cancelled,
                concurrency,
            )
            .await
    }
}

struct BoundPlacementReconciler {
    reconciler: FilesystemPlacementReconciler,
    meta: peryx_storage::meta::MetaStore,
    clock: Clock,
    authority: Option<Arc<dyn OwnershipAuthority>>,
}

struct GatedPlacementReconciler {
    reconciler: Arc<dyn peryx_ha::PlacementReconciler>,
    active: Arc<AtomicBool>,
}

struct GatedHomePlacementRecorder {
    recorder: Arc<dyn peryx_ha::HomePlacementRecorder>,
    active: Arc<AtomicBool>,
}

impl peryx_ha::HomePlacementRecorder for GatedHomePlacementRecorder {
    fn record(&self, digest: &str, size: u64, fence: u64) -> Result<(), String> {
        if !self.active.load(Ordering::Acquire) {
            return Err("distributed availability is not active".to_owned());
        }
        self.recorder.record(digest, size, fence)
    }
}

#[async_trait::async_trait]
impl peryx_ha::PlacementReconciler for GatedPlacementReconciler {
    async fn reconcile_pass(
        &self,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
        batch: std::num::NonZeroUsize,
    ) -> Result<AvailabilityTaskReport, AvailabilityTaskError> {
        ensure_active(&self.active)?;
        self.reconciler.reconcile_pass(cancelled, batch).await
    }
}

struct GatedBlobReclaimer {
    reclaimer: Arc<dyn peryx_ha::BlobReclaimer>,
    active: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl peryx_ha::BlobReclaimer for GatedBlobReclaimer {
    async fn reclaim_pass(
        &self,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
        fence: u64,
        batch: std::num::NonZeroUsize,
    ) -> Result<AvailabilityTaskReport, AvailabilityTaskError> {
        ensure_active(&self.active)?;
        self.reclaimer.reclaim_pass(cancelled, fence, batch).await
    }
}

fn ensure_active(active: &AtomicBool) -> Result<(), AvailabilityTaskError> {
    active
        .load(Ordering::Acquire)
        .then_some(())
        .ok_or_else(|| AvailabilityTaskError::new("availability_inactive", "distributed availability is not active"))
}

#[async_trait::async_trait]
impl peryx_ha::PlacementReconciler for BoundPlacementReconciler {
    async fn reconcile_pass(
        &self,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
        batch: std::num::NonZeroUsize,
    ) -> Result<AvailabilityTaskReport, AvailabilityTaskError> {
        self.reconciler.reconcile_pass(
            &self.meta,
            &self.clock,
            self.authority
                .as_ref()
                .map_or(0, |authority| authority.cluster_status().term),
            cancelled,
            batch,
        )
    }
}

/// # Errors
/// Returns an error when a configured datacenter or peer address is invalid.
pub fn assemble_workers(
    config: &RuntimeConfig,
    context: DistributedWorkerContext,
) -> anyhow::Result<AvailabilityCapabilities> {
    if !matches!(config.role, RuntimeRole::Primary { .. }) {
        return Ok(AvailabilityCapabilities::default());
    }
    let Some(local_identity) = config.node_identity.as_deref().or(config.writer_identity.as_deref()) else {
        return Ok(AvailabilityCapabilities::default());
    };
    let topology = ServiceTopology {
        membership: config.membership.clone(),
        local_identity: Some(local_identity.to_owned()),
    };
    let home_placement = local_member(&topology)
        .map(|(_, member)| {
            DataCenterId::new(&member.datacenter)
                .with_context(|| format!("local datacenter identity {}", member.datacenter))
        })
        .transpose()?
        .map(|data_center| {
            Arc::new(crate::placement_policy::DistributedHomePlacementRecorder::new(
                context.meta.clone(),
                context.backend.clone(),
                data_center,
                context.clock.clone(),
            )) as Arc<dyn peryx_ha::HomePlacementRecorder>
        });
    let (copier, placement) = match context.filesystem {
        Some(filesystem) => (
            cross_dc_blob_copier(
                &topology,
                config.role.token(),
                filesystem.clone(),
                context.backend.clone(),
            )?
            .map(|copier| {
                Arc::new(BoundCrossDcBlobCopier {
                    copier,
                    meta: context.meta.clone(),
                    clock: context.clock.clone(),
                    authority: context.authority.clone(),
                }) as Arc<dyn peryx_ha::CrossDcCopier>
            }),
            filesystem_placement_reconciler(&topology, filesystem)?.map(|reconciler| {
                Arc::new(BoundPlacementReconciler {
                    reconciler,
                    meta: context.meta.clone(),
                    clock: context.clock.clone(),
                    authority: context.authority.clone(),
                }) as Arc<dyn peryx_ha::PlacementReconciler>
            }),
        ),
        None => (None, None),
    };
    Ok(AvailabilityCapabilities {
        ownership: context.authority.clone(),
        copier,
        placement,
        home_placement,
        reclaimer: blob_reclamation_selector(&topology, context.references, context.frontiers)
            .map(|selector| selector.bind(context.meta, context.blobs, context.clock)),
    })
}

#[derive(Clone)]
struct RuntimeBindings {
    ownership: Option<Arc<DeferredOwnership>>,
    references: Arc<DeferredReferences>,
    frontiers: Arc<DeferredFrontiers>,
    active: Arc<AtomicBool>,
}

impl RuntimeBindings {
    fn new(owns_authority: bool) -> Self {
        let active = Arc::new(AtomicBool::new(false));
        Self {
            ownership: owns_authority.then(|| Arc::new(DeferredOwnership::new(active.clone()))),
            references: Arc::new(DeferredReferences::default()),
            frontiers: Arc::new(DeferredFrontiers::default()),
            active,
        }
    }

    fn gate(&self, capabilities: &mut AvailabilityCapabilities) {
        capabilities.ownership = self.authority();
        capabilities.copier = capabilities.copier.take().map(|copier| {
            Arc::new(GatedCrossDcCopier {
                copier,
                active: self.active.clone(),
            }) as Arc<dyn peryx_ha::CrossDcCopier>
        });
        capabilities.placement = capabilities.placement.take().map(|reconciler| {
            Arc::new(GatedPlacementReconciler {
                reconciler,
                active: self.active.clone(),
            }) as Arc<dyn peryx_ha::PlacementReconciler>
        });
        capabilities.home_placement = capabilities.home_placement.take().map(|recorder| {
            Arc::new(GatedHomePlacementRecorder {
                recorder,
                active: self.active.clone(),
            }) as Arc<dyn peryx_ha::HomePlacementRecorder>
        });
        capabilities.reclaimer = capabilities.reclaimer.take().map(|reclaimer| {
            Arc::new(GatedBlobReclaimer {
                reclaimer,
                active: self.active.clone(),
            }) as Arc<dyn peryx_ha::BlobReclaimer>
        });
    }

    fn authority(&self) -> Option<Arc<dyn OwnershipAuthority>> {
        self.ownership
            .as_ref()
            .map(|ownership| ownership.clone() as Arc<dyn OwnershipAuthority>)
    }

    fn bind_ownership(&self, ownership: Option<Arc<dyn OwnershipAuthority>>) {
        if let Some(binding) = &self.ownership {
            binding.bind(ownership);
        }
    }

    fn activate(&self) {
        self.active.store(true, Ordering::Release);
    }

    fn deactivate(&self) {
        self.active.store(false, Ordering::Release);
        self.bind_ownership(None);
    }
}

struct DeferredOwnership {
    ownership: std::sync::RwLock<Option<Arc<dyn OwnershipAuthority>>>,
    active: Arc<AtomicBool>,
}

impl DeferredOwnership {
    fn new(active: Arc<AtomicBool>) -> Self {
        Self {
            ownership: std::sync::RwLock::new(None),
            active,
        }
    }

    fn bind(&self, ownership: Option<Arc<dyn OwnershipAuthority>>) {
        *self
            .ownership
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = ownership;
    }

    fn current(&self) -> Option<Arc<dyn OwnershipAuthority>> {
        self.active.load(Ordering::Acquire).then(|| {
            self.ownership
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        })?
    }
}

#[async_trait::async_trait]
impl OwnershipAuthority for DeferredOwnership {
    async fn claim_home(&self, authority: &str) -> Result<peryx_ha::HomeClaim, peryx_ha::OwnershipError> {
        match self.current() {
            Some(ownership) => ownership.claim_home(authority).await,
            None => Err(peryx_ha::OwnershipError::Unavailable(
                "ownership is not active".to_owned(),
            )),
        }
    }

    fn cluster_status(&self) -> peryx_ha::ClusterStatus {
        self.current().map_or(
            peryx_ha::ClusterStatus {
                leader: None,
                term: 0,
                voters: Vec::new(),
            },
            |ownership| ownership.cluster_status(),
        )
    }

    async fn committed_epoch(&self, authority: &str) -> u64 {
        match self.current() {
            Some(ownership) => ownership.committed_epoch(authority).await,
            None => 0,
        }
    }

    async fn admit_epoch(&self, authority: &str, presented: u64) -> bool {
        match self.current() {
            Some(ownership) => ownership.admit_epoch(authority, presented).await,
            None => false,
        }
    }

    async fn begin_epoch_write(
        &self,
        authority: &str,
        presented: u64,
    ) -> Result<Option<peryx_ha::AuthorityWriteLease>, peryx_ha::OwnershipError> {
        match self.current() {
            Some(ownership) => ownership.begin_epoch_write(authority, presented).await,
            None => Err(peryx_ha::OwnershipError::Unavailable(
                "ownership is not active".to_owned(),
            )),
        }
    }

    async fn finish_epoch_write(&self, lease: &peryx_ha::AuthorityWriteLease) -> Result<(), peryx_ha::OwnershipError> {
        match self.current() {
            Some(ownership) => ownership.finish_epoch_write(lease).await,
            None => Err(peryx_ha::OwnershipError::Unavailable(
                "ownership is not active".to_owned(),
            )),
        }
    }

    async fn acquire_singleton_lease(
        &self,
        job: &str,
        holder: &str,
    ) -> Result<peryx_ha::SingletonAcquisition, peryx_ha::OwnershipError> {
        match self.current() {
            Some(ownership) => ownership.acquire_singleton_lease(job, holder).await,
            None => Err(peryx_ha::OwnershipError::Unavailable(
                "ownership is not active".to_owned(),
            )),
        }
    }

    async fn renew_singleton_lease(
        &self,
        lease: &peryx_ha::SingletonLease,
    ) -> Result<peryx_ha::SingletonRenewal, peryx_ha::OwnershipError> {
        match self.current() {
            Some(ownership) => ownership.renew_singleton_lease(lease).await,
            None => Err(peryx_ha::OwnershipError::Unavailable(
                "ownership is not active".to_owned(),
            )),
        }
    }

    async fn release_singleton_lease(
        &self,
        lease: &peryx_ha::SingletonLease,
    ) -> Result<peryx_ha::SingletonRelease, peryx_ha::OwnershipError> {
        match self.current() {
            Some(ownership) => ownership.release_singleton_lease(lease).await,
            None => Err(peryx_ha::OwnershipError::Unavailable(
                "ownership is not active".to_owned(),
            )),
        }
    }

    async fn transfer_home(
        &self,
        authority: &str,
        new_home: &str,
    ) -> Result<Option<peryx_ha::TransferOutcome>, peryx_ha::OwnershipError> {
        match self.current() {
            Some(ownership) => ownership.transfer_home(authority, new_home).await,
            None => Err(peryx_ha::OwnershipError::Unavailable(
                "ownership is not active".to_owned(),
            )),
        }
    }

    async fn pending_transfer_audits(&self) -> Result<Vec<peryx_ha::PendingTransferAudit>, peryx_ha::OwnershipError> {
        match self.current() {
            Some(ownership) => ownership.pending_transfer_audits().await,
            None => Err(peryx_ha::OwnershipError::Unavailable(
                "ownership is not active".to_owned(),
            )),
        }
    }

    async fn complete_transfer_audit(&self, id: &str) -> Result<(), peryx_ha::OwnershipError> {
        match self.current() {
            Some(ownership) => ownership.complete_transfer_audit(id).await,
            None => Err(peryx_ha::OwnershipError::Unavailable(
                "ownership is not active".to_owned(),
            )),
        }
    }
}

#[derive(Default)]
struct DeferredReferences(std::sync::RwLock<Option<Arc<dyn ReferenceInventory>>>);

impl DeferredReferences {
    fn bind(&self, references: Arc<dyn ReferenceInventory>) {
        *self.0.write().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(references);
    }
}

impl ReferenceInventory for DeferredReferences {
    fn referenced(&self) -> Result<BTreeSet<String>, String> {
        self.0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .ok_or_else(|| "reference inventory is not active".to_owned())?
            .referenced()
    }
}

#[derive(Default)]
struct DeferredFrontiers(std::sync::RwLock<Option<Arc<dyn ReclamationFrontiers>>>);

impl DeferredFrontiers {
    fn bind(&self, frontiers: Arc<dyn ReclamationFrontiers>) {
        *self.0.write().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(frontiers);
    }
}

impl ReclamationFrontiers for DeferredFrontiers {
    fn observe(&self) -> Option<peryx_ha::ObservedFrontier> {
        self.0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(|frontiers| frontiers.observe())
    }
}

pub fn availability_control_router(config: &RuntimeConfig, context: AvailabilityControlContext) -> Router {
    let frontier = Arc::new(RosterFrontierSource::new(
        config
            .membership
            .as_ref()
            .map(|membership| datacenter_roster(membership, None).into_iter().collect())
            .unwrap_or_default(),
        config.role.token(),
    ));
    availability_router(ControlHttpContext {
        authorizer: context.authorizer,
        posture: AvailabilityPosture::new(
            config.mode,
            match &config.role {
                RuntimeRole::Primary { .. } => AvailabilityPostureRole::Writer,
                RuntimeRole::Replica { .. } => AvailabilityPostureRole::Replica,
            },
        ),
        read_only: context.read_only,
        meta: context.meta,
        control: context.control,
        ownership: context.ownership,
        coordinator: Arc::new(TransferCoordinator::new(frontier)),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ServiceTopology {
    membership: Option<RuntimeMembership>,
    local_identity: Option<String>,
}

/// # Errors
/// Returns an error when a datacenter or peer address is invalid.
fn cross_dc_blob_copier(
    topology: &ServiceTopology,
    token: &str,
    store: BlobStore,
    backend: BackendId,
) -> anyhow::Result<Option<CrossDcBlobCopier>> {
    let Some((membership, local)) = local_member(topology) else {
        return Ok(None);
    };
    let local_dc =
        DataCenterId::new(&local.datacenter).context("the local datacenter is not a valid placement component")?;
    CrossDcBlobCopier::http(
        local_dc,
        source_roster(membership, &local.datacenter),
        token,
        store,
        backend,
    )
    .map_err(Into::into)
}

/// # Errors
/// Returns an error when a datacenter is invalid.
fn filesystem_placement_reconciler(
    topology: &ServiceTopology,
    store: BlobStore,
) -> anyhow::Result<Option<FilesystemPlacementReconciler>> {
    let Some((membership, local)) = local_member(topology) else {
        return Ok(None);
    };
    let local_dc =
        DataCenterId::new(&local.datacenter).context("the local datacenter is not a valid placement component")?;
    let target_dcs = membership
        .members
        .iter()
        .map(|member| {
            DataCenterId::new(&member.datacenter)
                .with_context(|| format!("datacenter {:?} is not a valid placement component", member.datacenter))
        })
        .collect::<anyhow::Result<BTreeSet<_>>>()?;
    Ok(FilesystemPlacementReconciler::new(local_dc, store, target_dcs))
}

#[must_use]
fn blob_reclamation_selector(
    topology: &ServiceTopology,
    references: Arc<dyn ReferenceInventory>,
    frontiers: Arc<dyn ReclamationFrontiers>,
) -> Option<BlobReclamationSelector> {
    local_member(topology).map(|_| BlobReclamationSelector::new(references, frontiers))
}

fn local_member(topology: &ServiceTopology) -> Option<(&crate::RuntimeMembership, &crate::RuntimeMember)> {
    let identity = topology.local_identity.as_deref()?;
    let membership = topology.membership.as_ref()?;
    membership
        .members
        .iter()
        .find(|member| member.node == identity)
        .map(|member| (membership, member))
}

fn source_roster(membership: &crate::RuntimeMembership, local_dc: &str) -> HashMap<String, String> {
    datacenter_roster(membership, Some(local_dc))
}

pub fn datacenter_roster(membership: &crate::RuntimeMembership, excluded_dc: Option<&str>) -> HashMap<String, String> {
    let mut roster = HashMap::new();
    for member in &membership.members {
        if excluded_dc.is_some_and(|excluded| member.datacenter == excluded) {
            continue;
        }
        match roster.entry(member.datacenter.clone()) {
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(member.address.clone());
            }
            std::collections::hash_map::Entry::Occupied(mut slot) if member.role == RuntimeMemberRole::Writer => {
                slot.insert(member.address.clone());
            }
            std::collections::hash_map::Entry::Occupied(_) => {}
        }
    }
    roster
}

fn topology(config: &RuntimeConfig, read_only: bool) -> TopologyConfig {
    let (group, members) = config.membership.as_ref().map_or_else(
        || (None, Vec::new()),
        |membership| {
            (
                Some(membership.group.clone()),
                membership
                    .members
                    .iter()
                    .map(|member| TopologyMember {
                        node: member.node.clone(),
                        dc: member.datacenter.clone(),
                        address: member.address.clone(),
                        role: runtime_member_role(member.role),
                    })
                    .collect(),
            )
        },
    );
    TopologyConfig {
        mode: match config.mode {
            DistributedMode::Dc => TopologyMode::Dc,
            DistributedMode::Ha => TopologyMode::Ha,
        },
        group,
        members,
        local_node: (!read_only).then(|| config.writer_identity.clone()).flatten(),
    }
}

const fn runtime_role(role: &RuntimeRole) -> NodeRole {
    match role {
        RuntimeRole::Primary { .. } => NodeRole::Writer,
        RuntimeRole::Replica { .. } => NodeRole::Replica,
    }
}

const fn runtime_member_role(role: RuntimeMemberRole) -> NodeRole {
    match role {
        RuntimeMemberRole::Writer => NodeRole::Writer,
        RuntimeMemberRole::Replica => NodeRole::Replica,
    }
}

fn receipt_sources(config: &RuntimeConfig) -> anyhow::Result<Vec<Arc<dyn ReceiptSource + Send + Sync>>> {
    let (Some(membership), Some(identity)) = (
        config.membership.as_ref(),
        config.node_identity.as_deref().or(config.writer_identity.as_deref()),
    ) else {
        return Ok(Vec::new());
    };
    let Some(local) = membership.members.iter().find(|member| member.node == identity) else {
        return Ok(Vec::new());
    };
    membership
        .members
        .iter()
        .filter(|member| member.datacenter == local.datacenter && member.node != identity)
        .map(|member| {
            HttpReceiptSource::new(
                &member.address,
                member.node.clone(),
                config.role.token(),
                RECEIPT_FETCH_TIMEOUT,
            )
            .map(|source| Arc::new(source) as Arc<dyn ReceiptSource + Send + Sync>)
            .with_context(|| format!("build receipt transport for peer {}", member.node))
        })
        .collect()
}

fn remote_frontier_sources(config: &RuntimeConfig) -> anyhow::Result<Vec<Arc<dyn RemoteFrontierSource + Send + Sync>>> {
    if config.mode != DistributedMode::Ha {
        return Ok(Vec::new());
    }
    let (Some(membership), Some(identity)) = (
        config.membership.as_ref(),
        config.node_identity.as_deref().or(config.writer_identity.as_deref()),
    ) else {
        return Ok(Vec::new());
    };
    let Some(local) = membership.members.iter().find(|member| member.node == identity) else {
        return Ok(Vec::new());
    };
    datacenter_roster(membership, Some(&local.datacenter))
        .into_iter()
        .map(|(datacenter, address)| {
            HttpRemoteFrontierSource::new(
                &member_base_url(&address),
                datacenter.clone(),
                config.role.token(),
                RECEIPT_FETCH_TIMEOUT,
            )
            .map(|source| Arc::new(source) as Arc<dyn RemoteFrontierSource + Send + Sync>)
            .with_context(|| format!("build remote frontier transport for datacenter {datacenter}"))
        })
        .collect()
}

fn member_base_url(address: &str) -> String {
    if address.starts_with("http://") || address.starts_with("https://") {
        address.to_owned()
    } else {
        format!("http://{address}")
    }
}

#[cfg(test)]
#[path = "../tests/unit/service_assembly_tests.rs"]
mod tests;
