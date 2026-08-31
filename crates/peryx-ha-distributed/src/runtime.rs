use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::raft::network::DeferredRaftRpcHandler;
use crate::read_through::{DEFAULT_READ_THROUGH_LIMITS, DcTransport, ReadThroughLimits, RemotePlacementReader};
use crate::{
    AnalyticsPuller, AvailabilityMetrics, AvailabilityRuntime, BeaconSender, CapacityLimited, ConsensusMember,
    ConsensusPlan, DEFAULT_BEACON_INTERVAL, DEFAULT_DEAD_AFTER, DEFAULT_RECONNECT_POLICY, DEFAULT_SET_LIMITS,
    DEFAULT_SUSPECT_AFTER, DEFAULT_TRANSFER_LIMITS, DurabilityPolicy, HttpBlobTransport, HttpPeerTransport,
    LivenessTracker, MemberFrontier, MemberRole, OwnershipGroup, PeerSet, REPLICA_BLOB_FETCH_CONCURRENCY, ReplicaLoop,
    ReplicaLoopParts, ReplicaMonitor, ReplicaReclamationFrontiers, SetLimits, TransferLimits, WorkerShared,
    analytics_router, follower_router, frontier_router, group_readiness, liveness_router, primary_router,
    receipt_router, resolve_producer_epoch,
};
use anyhow::Context as _;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse as _, Response};
use axum::routing::get;
use axum::{Json, Router};
use peryx_core::{Clock, PrometheusSource};
use peryx_ha::{
    AnalyticsBatchSource, AvailabilityAudience, AvailabilityAuthorizer, OwnershipAuthority, ReplicaViewApplier,
};
use peryx_storage::blob::BlobStorage;
use peryx_storage::meta::{DataCenterId, MetaStore};
use peryx_upstream::redact_url;
use serde_json::{Value, json};

const LOG_STORE_SUBPATH: &str = "raft/ownership-log.redb";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DistributedMode {
    Dc,
    Ha,
}

impl DistributedMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dc => "dc",
            Self::Ha => "ha",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeMemberRole {
    Writer,
    Replica,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeMember {
    pub node: String,
    pub datacenter: String,
    pub address: String,
    pub role: RuntimeMemberRole,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeMembership {
    pub group: String,
    pub members: Vec<RuntimeMember>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeRole {
    Primary {
        source: String,
        token: String,
    },
    Replica {
        upstream: String,
        token: String,
        poll_interval: Duration,
        page_size: NonZeroUsize,
    },
}

impl RuntimeRole {
    pub(crate) fn token(&self) -> &str {
        match self {
            Self::Primary { token, .. } | Self::Replica { token, .. } => token,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub mode: DistributedMode,
    pub role: RuntimeRole,
    pub write_ack_policy: DurabilityPolicy,
    pub membership: Option<RuntimeMembership>,
    pub node_identity: Option<String>,
    pub writer_identity: Option<String>,
    pub data_dir: PathBuf,
    pub read_through: Option<ReadThroughLimits>,
}

#[derive(Clone, Copy)]
enum AvailabilityRole {
    Primary,
    Replica,
}

impl AvailabilityRole {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Replica => "replica",
        }
    }
}

/// Stores the primary origin after stripping credentials, query, and fragment.
#[derive(Clone)]
struct ReplicaView {
    monitor: Arc<ReplicaMonitor>,
    upstream: String,
}

/// A worker panic fails readiness while the replica continues serving under its staleness contract.
fn worker_reason(workers: Option<&Arc<WorkerShared>>) -> Option<&'static str> {
    workers
        .filter(|workers| !workers.is_healthy())
        .map(|_| "worker_unhealthy")
}

#[derive(Clone)]
struct AvailabilityNode {
    meta: MetaStore,
    blobs: BlobStorage,
    authorizer: Arc<dyn AvailabilityAuthorizer>,
    mode: &'static str,
    role: AvailabilityRole,
    replica: Option<ReplicaView>,
    /// Replica liveness informs routing hints but does not gate the writer's readiness.
    liveness: Option<Arc<LivenessTracker>>,
    /// Replicas and rosterless writers publish no group readiness.
    group: Option<GroupReadinessSource>,
    workers: Option<Arc<WorkerShared>>,
}

#[derive(Clone)]
struct GroupReadinessSource {
    members: Vec<(String, MemberRole)>,
    policy: DurabilityPolicy,
}

fn group_readiness_source(config: &RuntimeConfig) -> Option<GroupReadinessSource> {
    let members = config
        .membership
        .as_ref()?
        .members
        .iter()
        .map(|member| {
            let role = match member.role {
                RuntimeMemberRole::Writer => MemberRole::Writer,
                RuntimeMemberRole::Replica => MemberRole::Replica,
            };
            (member.node.clone(), role)
        })
        .collect();
    Some(GroupReadinessSource {
        members,
        policy: config.write_ack_policy,
    })
}

impl AvailabilityNode {
    /// Readiness requires a healthy blob store, replica frontier, and worker domain.
    async fn readiness(&self) -> (bool, Vec<&'static str>) {
        let mut reasons = Vec::new();
        if self.blobs.health().await.is_err() {
            reasons.push("blob_store");
        }
        if let Some(gap) = self
            .replica
            .as_ref()
            .and_then(|replica| replica.monitor.readiness_gap())
        {
            reasons.push(gap);
        }
        reasons.extend(worker_reason(self.workers.as_ref()));
        (reasons.is_empty(), reasons)
    }

    /// Operators receive frontiers and lag; administrators also receive the redacted primary origin.
    async fn document(&self, audience: AvailabilityAudience) -> (bool, serde_json::Map<String, Value>) {
        let (ready, reasons) = self.readiness().await;
        let mut body = serde_json::Map::from_iter([
            ("mode".to_owned(), json!(self.mode)),
            ("role".to_owned(), json!(self.role.as_str())),
            ("ready".to_owned(), json!(ready)),
            ("reasons".to_owned(), json!(reasons)),
        ]);
        if let Some(replica) = &self.replica {
            let observation = replica.monitor.snapshot();
            let lag = observation
                .primary_serial
                .map(|primary_serial| primary_serial.saturating_sub(observation.serial));
            if audience >= AvailabilityAudience::Operator {
                body.extend([
                    ("serial".to_owned(), json!(observation.serial)),
                    ("primary_serial".to_owned(), json!(observation.primary_serial)),
                    ("lag".to_owned(), json!(lag)),
                    ("synced_changes".to_owned(), json!(observation.changes)),
                    ("sync_errors".to_owned(), json!(observation.errors)),
                    ("retired_members".to_owned(), json!(observation.retired)),
                ]);
            }
            if audience == AvailabilityAudience::Administrator {
                body.insert("upstream".to_owned(), json!(replica.upstream));
            }
        } else {
            let serial = self.meta.current_serial().unwrap_or(0);
            if audience >= AvailabilityAudience::Operator {
                body.insert("serial".to_owned(), json!(serial));
                let now = Instant::now();
                if let Some(liveness) = &self.liveness {
                    body.insert("peers".to_owned(), json!(liveness.summary(now)));
                }
                if let Some(group) = &self.group {
                    body.insert("group_readiness".to_owned(), self.group_readiness(group, serial, now));
                }
            }
        }
        (ready, body)
    }

    /// Silent members remain in the quorum denominator but contribute no frontier.
    fn group_readiness(&self, group: &GroupReadinessSource, serial: u64, now: Instant) -> Value {
        let members: Vec<MemberFrontier> = group
            .members
            .iter()
            .map(|(node, role)| {
                let applied = match role {
                    MemberRole::Writer => Some(serial),
                    MemberRole::Replica => self
                        .liveness
                        .as_ref()
                        .and_then(|liveness| liveness.applied_frontier(node, now)),
                };
                MemberFrontier {
                    member: node.clone(),
                    role: *role,
                    applied,
                }
            })
            .collect();
        let readiness = group_readiness(&members, group.policy);
        json!({
            "ready": readiness.is_ready(),
            "durable_frontier": readiness.durable_frontier,
            "policy": group.policy.as_str(),
            "blocked": readiness.blocked,
        })
    }
}

/// Returns `200` while the process serves, including while a replica catches up.
async fn availability_health(State(node): State<AvailabilityNode>, headers: HeaderMap) -> Response {
    let authorization = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    let Ok(audience) = node.authorizer.authorize(authorization).await else {
        return unavailable();
    };
    let (_ready, body) = node.document(audience).await;
    availability_response(StatusCode::OK, body)
}

/// Returns `503` for a frontier gap, incompatible schema, or failed local store.
async fn availability_readiness(State(node): State<AvailabilityNode>, headers: HeaderMap) -> Response {
    let authorization = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    let Ok(audience) = node.authorizer.authorize(authorization).await else {
        return unavailable();
    };
    let (ready, body) = node.document(audience).await;
    let status = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    availability_response(status, body)
}

fn availability_response(status: StatusCode, body: serde_json::Map<String, Value>) -> Response {
    let mut response = (status, Json(Value::Object(body))).into_response();
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}

fn unavailable() -> Response {
    availability_response(
        StatusCode::SERVICE_UNAVAILABLE,
        serde_json::Map::from_iter([("error".to_owned(), json!("identity service unavailable"))]),
    )
}

/// A timed-out blob request is a retryable transport loss.
const BLOB_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

const METADATA_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Installs read-through when the node can identify its datacenter and at least one remote peer.
///
/// # Errors
/// Returns an error if the replication token cannot be read or a peer address is not a usable HTTP base.
pub fn remote_blob_availability(
    config: &RuntimeConfig,
    meta: MetaStore,
    blobs: BlobStorage,
    clock: Clock,
) -> anyhow::Result<Option<Arc<dyn peryx_ha::BlobAvailability>>> {
    let Some(membership) = config.membership.as_ref() else {
        return Ok(None);
    };
    let Some(local_dc) = local_datacenter(config, membership) else {
        return Ok(None);
    };
    let dc = DataCenterId::new(local_dc.clone()).with_context(|| format!("local datacenter identity {local_dc}"))?;
    let limits = config.read_through.unwrap_or(DEFAULT_READ_THROUGH_LIMITS);
    let built = peer_blob_delegates(membership, &local_dc, config.role.token(), limits)?;
    if built.is_empty() {
        return Ok(None);
    }
    let delegates: HashMap<String, DcTransport> = built
        .into_iter()
        .map(|(dc, transport)| (dc, Arc::new(transport) as DcTransport))
        .collect();
    Ok(Some(Arc::new(RemotePlacementReader::new(
        meta, blobs, dc, delegates, limits, clock,
    ))))
}

/// # Errors
/// Returns an error when a member address is not a usable HTTP base.
fn peer_blob_delegates(
    membership: &RuntimeMembership,
    local_dc: &str,
    token: &str,
    limits: ReadThroughLimits,
) -> anyhow::Result<HashMap<String, CapacityLimited<HttpBlobTransport>>> {
    let transfer = TransferLimits {
        max_operations: TransferLimits::default().max_operations,
        max_encoded_bytes: limits.per_fetch_bytes,
    };
    let writers: HashSet<&str> = membership
        .members
        .iter()
        .filter(|member| member.role == RuntimeMemberRole::Writer)
        .map(|member| member.datacenter.as_str())
        .collect();
    let mut delegates = HashMap::new();
    for (datacenter, address) in crate::service_assembly::datacenter_roster(membership, Some(local_dc)) {
        let base = peer_blob_base(&address);
        let transport = HttpBlobTransport::new(&base, token.to_owned(), transfer, BLOB_FETCH_TIMEOUT)
            .with_context(|| format!("build a read-through blob transport for datacenter {datacenter}"))?;
        if !writers.contains(datacenter.as_str()) {
            continue;
        }
        delegates.insert(datacenter, CapacityLimited::new(transport, limits.concurrency));
    }
    Ok(delegates)
}

/// Without a resolved local datacenter, the replica defers nothing and pulls each blob in full.
///
/// Metadata peer construction validates these addresses before blob transport reconstruction.
fn replica_blob_deferral(
    config: &RuntimeConfig,
    token: &str,
) -> anyhow::Result<(String, HashMap<String, CapacityLimited<HttpBlobTransport>>)> {
    let local = config
        .membership
        .as_ref()
        .and_then(|membership| local_datacenter(config, membership));
    let (Some(membership), Some(local_dc)) = (config.membership.as_ref(), local) else {
        return Ok((String::new(), HashMap::new()));
    };
    let limits = config.read_through.unwrap_or(DEFAULT_READ_THROUGH_LIMITS);
    let delegates = peer_blob_delegates(membership, &local_dc, token, limits)?;
    Ok((local_dc, delegates))
}

/// Bare peer addresses use HTTP; the function preserves explicit HTTP or HTTPS schemes.
fn peer_blob_base(address: &str) -> String {
    if address.starts_with("http://") || address.starts_with("https://") {
        address.to_owned()
    } else {
        format!("http://{address}")
    }
}

/// `writer_identity` supplies the local datacenter when `node_identity` is absent.
fn local_datacenter(config: &RuntimeConfig, membership: &RuntimeMembership) -> Option<String> {
    let identity = config.node_identity.as_deref().or(config.writer_identity.as_deref())?;
    membership
        .members
        .iter()
        .find(|member| member.node == identity)
        .map(|member| member.datacenter.clone())
}

const UPSTREAM_SOURCE: &str = "upstream";

/// Returns an error when a member address is not a usable peer URL.
fn metadata_peers(
    membership: Option<&RuntimeMembership>,
    this: Option<&str>,
    upstream: &str,
    token: &str,
    resume: u64,
    page_size: std::num::NonZeroUsize,
) -> anyhow::Result<PeerSet<HttpPeerTransport>> {
    let limits = SetLimits {
        request_size: page_size,
        ..DEFAULT_SET_LIMITS
    };
    let mut set = PeerSet::new(limits, DEFAULT_RECONNECT_POLICY);
    let mut joined = std::collections::BTreeSet::new();
    let transport_limits = TransferLimits {
        max_operations: page_size,
        ..DEFAULT_TRANSFER_LIMITS
    };
    if let Some(membership) = membership {
        for member in &membership.members {
            if Some(member.node.as_str()) == this || !joined.insert(member.address.clone()) {
                continue;
            }
            let transport = HttpPeerTransport::new(&member.address, token, transport_limits, METADATA_FETCH_TIMEOUT)
                .with_context(|| format!("build the metadata peer transport for {}", member.node))?;
            set.join(member.node.clone(), transport, resume);
        }
    }
    if joined.insert(upstream.to_owned()) {
        let transport = HttpPeerTransport::new(upstream, token, transport_limits, METADATA_FETCH_TIMEOUT)
            .context("build the upstream metadata transport")?;
        set.join(UPSTREAM_SOURCE, transport, resume);
    }
    Ok(set)
}

/// # Errors
/// Returns an error when a peer address or the upstream URL is not a usable base.
fn replica_transports(
    config: &RuntimeConfig,
    upstream: &str,
    token: &str,
    resume: u64,
    page_size: std::num::NonZeroUsize,
) -> anyhow::Result<(PeerSet<HttpPeerTransport>, CapacityLimited<HttpBlobTransport>)> {
    let metadata = metadata_peers(
        config.membership.as_ref(),
        config.node_identity.as_deref(),
        upstream,
        token,
        resume,
        page_size,
    )
    .context("build the replica metadata peer set")?;
    let blob_transport = HttpBlobTransport::new(
        upstream,
        token.to_owned(),
        TransferLimits::default(),
        BLOB_FETCH_TIMEOUT,
    )
    .context("build replica blob transport")?;
    Ok((
        metadata,
        CapacityLimited::new(blob_transport, REPLICA_BLOB_FETCH_CONCURRENCY),
    ))
}

/// # Errors
/// Propagates a token read failure or a route-build failure.
fn build_primary(
    config: &RuntimeConfig,
    context: &DistributedRuntimeContext,
    mode: DistributedMode,
    source: &str,
    token: &str,
    authorizer: Arc<dyn AvailabilityAuthorizer>,
) -> anyhow::Result<(Router, AvailabilityNode)> {
    let router = primary_router(
        source.to_owned(),
        token.to_owned(),
        context.meta.clone(),
        context.blobs.clone(),
    )
    .context("build primary replication routes")?;
    let router = merge_analytics_endpoint(router, config, context, token)?;
    let liveness = primary_liveness(config);
    let router = match &liveness {
        Some(tracker) => {
            router.merge(liveness_router(token.to_owned(), tracker.clone()).context("build liveness ingest routes")?)
        }
        None => router,
    };
    let node = AvailabilityNode {
        meta: context.meta.clone(),
        blobs: context.blobs.clone(),
        authorizer,
        mode: mode.as_str(),
        role: AvailabilityRole::Primary,
        replica: None,
        liveness,
        group: group_readiness_source(config),
        workers: None,
    };
    Ok((router, node))
}

/// Reuses the upstream and token already validated for the metadata transport.
fn replica_beacon(
    config: &RuntimeConfig,
    context: &DistributedRuntimeContext,
    upstream: &str,
    token: &str,
    metrics: Arc<AvailabilityMetrics>,
) -> Option<BeaconSender> {
    let node = config.node_identity.as_deref()?;
    Some(
        BeaconSender::new(
            upstream,
            token,
            node,
            u64::try_from((context.clock)()).unwrap_or(0),
            context.meta.clone(),
            DEFAULT_BEACON_INTERVAL,
        )
        .expect("the validated upstream and token also build the frontier beacon")
        .with_metrics(metrics),
    )
}

fn primary_liveness(config: &RuntimeConfig) -> Option<Arc<LivenessTracker>> {
    let replicas: Vec<String> = config
        .membership
        .as_ref()?
        .members
        .iter()
        .filter(|member| member.role == RuntimeMemberRole::Replica)
        .map(|member| member.node.clone())
        .collect();
    (!replicas.is_empty()).then(|| {
        Arc::new(LivenessTracker::new(
            replicas,
            DEFAULT_SUSPECT_AFTER,
            DEFAULT_DEAD_AFTER,
        ))
    })
}

pub struct DistributedRuntime {
    routes: Router,
    replica: Option<(ReplicaLoop, Arc<WorkerShared>)>,
    availability: AvailabilityNode,
    analytics_puller: Option<AnalyticsPuller>,
    consensus: Option<PlannedConsensus>,
    beacon: Option<BeaconSender>,
    prometheus: Vec<Arc<dyn PrometheusSource>>,
    clock: Clock,
}

/// Pairs the plan with the binding its ignited node fills in, so the mounted peer routes and the
/// consensus that answers them cannot exist apart.
struct PlannedConsensus {
    plan: ConsensusPlan,
    peer_rpc: Arc<DeferredRaftRpcHandler>,
}

pub struct PreparedDistributedRuntime {
    runtime: DistributedRuntime,
    worker: PreparedWorker,
}

enum PreparedWorker {
    Primary,
    Replica {
        replica: Box<ReplicaLoop>,
        runtime: AvailabilityRuntime,
    },
}

#[derive(Clone)]
pub struct DistributedRuntimeContext {
    pub meta: MetaStore,
    pub blobs: BlobStorage,
    pub clock: Clock,
    pub replica_views: Arc<dyn ReplicaViewApplier>,
    pub analytics: Arc<dyn AnalyticsBatchSource>,
    pub frontier: Arc<dyn crate::MetadataFrontierProvider>,
}

pub struct Consensus {
    pub authority: Arc<dyn OwnershipAuthority>,
    pub control: Arc<dyn peryx_ha::ControlExecutor>,
    peer_rpc: Arc<DeferredRaftRpcHandler>,
    owner: ConsensusOwner,
}

enum ConsensusOwner {
    Running {
        group: Arc<OwnershipGroup>,
        executor: crate::consensus_runtime::RaftExecutor,
    },
}

impl Consensus {
    /// Unbinds the peer routes first: they hold the raft handle, and a served RPC after cancellation
    /// would resurrect work the shutdown is draining.
    pub(crate) fn cancel(&self) {
        self.peer_rpc.bind(None);
        let ConsensusOwner::Running { executor, .. } = &self.owner;
        executor.cancel();
    }

    pub(crate) fn shutdown(self) -> anyhow::Result<()> {
        let ConsensusOwner::Running { group, executor } = self.owner;
        let result = executor.shutdown_and_join();
        drop(group);
        result
    }
}

/// A receipt attests which member holds the bytes, so a process that cannot name itself must not serve
/// one: peers would otherwise count an anonymous answer as a distinct copy.
fn merge_receipt_endpoint(
    router: Router,
    config: &RuntimeConfig,
    context: &DistributedRuntimeContext,
) -> anyhow::Result<Router> {
    let Some(identity) = config.node_identity.as_ref().or(config.writer_identity.as_ref()) else {
        return Ok(router);
    };
    Ok(router.merge(
        receipt_router(config.role.token().to_owned(), identity.clone(), context.blobs.clone())
            .context("build receipt routes")?,
    ))
}

/// HA producers use their member identity; a DC primary is the configured writer.
fn merge_analytics_endpoint(
    router: Router,
    config: &RuntimeConfig,
    context: &DistributedRuntimeContext,
    token: &str,
) -> anyhow::Result<Router> {
    let Some(identity) = config.node_identity.as_ref().or(config.writer_identity.as_ref()) else {
        return Ok(router);
    };
    let epoch = resolve_producer_epoch(&context.meta.analytics()).context("resolve the analytics producer epoch")?;
    Ok(router.merge(analytics_router(
        token.to_owned(),
        context.analytics.clone(),
        crate::ProducerId(identity.clone()),
        epoch,
    )))
}

fn build_analytics_puller(
    config: &RuntimeConfig,
    context: &DistributedRuntimeContext,
) -> anyhow::Result<Option<AnalyticsPuller>> {
    let RuntimeRole::Replica {
        upstream,
        token,
        poll_interval,
        ..
    } = &config.role
    else {
        return Ok(None);
    };
    let puller = AnalyticsPuller::new(upstream, token, context.meta.analytics(), *poll_interval)
        .context("build the analytics pull worker")?;
    Ok(Some(puller))
}

impl DistributedRuntime {
    #[must_use]
    pub fn reclamation_frontiers(&self) -> Arc<dyn peryx_ha::ReclamationFrontiers> {
        let replicas = self
            .availability
            .group
            .as_ref()
            .map(|group| {
                group
                    .members
                    .iter()
                    .filter(|(_, role)| matches!(role, MemberRole::Replica))
                    .map(|(node, _)| node.clone())
                    .collect()
            })
            .unwrap_or_default();
        Arc::new(ReplicaReclamationFrontiers::new(
            self.availability.liveness.clone(),
            replicas,
        ))
    }

    /// # Errors
    /// Returns an error if a secret cannot be read, the upstream URL is invalid, or the primary
    /// router rejects its identity or token.
    pub fn new(
        config: &RuntimeConfig,
        context: &DistributedRuntimeContext,
        authorizer: Arc<dyn AvailabilityAuthorizer>,
    ) -> anyhow::Result<Self> {
        let (routes, replica, availability, beacon, prometheus) = match &config.role {
            RuntimeRole::Primary { source, token } => {
                let (router, node) = build_primary(config, context, config.mode, source, token, authorizer.clone())?;
                (router, None, node, None, Vec::new())
            }
            RuntimeRole::Replica {
                upstream,
                token,
                poll_interval,
                page_size,
            } => {
                let resume = context.meta.current_serial().context("read the replica serial")?;
                let (metadata, transport) = replica_transports(config, upstream, token, resume, *page_size)?;
                let (local_dc, delegates) = replica_blob_deferral(config, token)?;
                let follower = follower_router(token.clone(), context.meta.clone())
                    .context("build the follower change-feed routes")?;
                let monitor = Arc::new(ReplicaMonitor::new(resume));
                let metrics = Arc::new(AvailabilityMetrics::default());
                let beacon = replica_beacon(config, context, upstream, token, metrics.clone());
                let workers = Arc::new(WorkerShared::for_replica());
                let node = AvailabilityNode {
                    meta: context.meta.clone(),
                    blobs: context.blobs.clone(),
                    authorizer,
                    mode: config.mode.as_str(),
                    role: AvailabilityRole::Replica,
                    replica: Some(ReplicaView {
                        monitor: monitor.clone(),
                        upstream: redact_url(upstream),
                    }),
                    liveness: None,
                    group: None,
                    workers: Some(workers.clone()),
                };
                let prometheus: Vec<Arc<dyn PrometheusSource>> =
                    vec![monitor.clone(), metrics.clone(), workers.clone()];
                (
                    follower,
                    Some((
                        ReplicaLoop::new(ReplicaLoopParts {
                            views: context.replica_views.clone(),
                            metadata,
                            policy: DEFAULT_RECONNECT_POLICY,
                            meta: context.meta.clone(),
                            blobs: context.blobs.clone(),
                            page_size: *page_size,
                            poll_interval: *poll_interval,
                            monitor,
                            metrics,
                            transport,
                            local_dc,
                            delegates,
                        }),
                        workers,
                    )),
                    node,
                    beacon,
                    prometheus,
                )
            }
        };
        let routes = merge_receipt_endpoint(routes, config, context)?;
        let routes = if config.mode == DistributedMode::Ha {
            routes.merge(
                frontier_router(config.role.token().to_owned(), context.frontier.clone())
                    .context("build frontier routes")?,
            )
        } else {
            routes
        };
        let analytics_puller = build_analytics_puller(config, context)?;
        // A member address names one plane, so the peer RPCs join the routes every other peer
        // transport dials. The raft node behind them only exists once consensus ignites.
        let (routes, consensus) = match consensus_plan(config)? {
            Some(plan) => {
                let peer_rpc = Arc::new(DeferredRaftRpcHandler::default());
                let peer = crate::raft::network::raft_rpc_router(plan.local_voter(), plan.token(), peer_rpc.clone())
                    .context("build the peer raft rpc routes")?;
                (routes.merge(peer), Some(PlannedConsensus { plan, peer_rpc }))
            }
            None => (routes, None),
        };
        Ok(Self {
            routes,
            replica,
            availability,
            analytics_puller,
            consensus,
            beacon,
            prometheus,
            clock: context.clock.clone(),
        })
    }

    /// # Errors
    /// Returns an error when opening the consensus log store, starting the node, or bootstrapping fails.
    pub(crate) async fn ignite_consensus_with_lifecycle(
        &self,
        lifecycle: crate::lifecycle::Lifecycle,
    ) -> anyhow::Result<Option<Consensus>> {
        match &self.consensus {
            Some(consensus) => {
                let started = consensus.plan.ignite_with_lifecycle(lifecycle).await?;
                consensus
                    .peer_rpc
                    .bind(Some(started.node().rpc_handler_with_clock(self.clock.clone())));
                let (node, executor) = started.commit();
                let group = Arc::new(
                    OwnershipGroup::new(node, consensus.plan.home())
                        .with_peer_forwarding(consensus.plan.token())
                        .with_clock(self.clock.clone()),
                );
                let ownership = Arc::new(crate::consensus_runtime::OwnershipHandle::new(&group));
                Ok(Some(Consensus {
                    authority: ownership.clone(),
                    control: Arc::new(crate::ControlPlane::new(ownership, self.clock.clone())),
                    peer_rpc: consensus.peer_rpc.clone(),
                    owner: ConsensusOwner::Running { group, executor },
                }))
            }
            None => Ok(None),
        }
    }

    pub(crate) const fn requires_control_listener(&self) -> bool {
        self.consensus.is_some()
    }

    pub(crate) fn prepare_worker_runtime(mut self) -> std::io::Result<PreparedDistributedRuntime> {
        let worker = match self.replica.take() {
            Some((replica, workers)) => PreparedWorker::Replica {
                replica: Box::new(replica),
                runtime: AvailabilityRuntime::start(workers)?,
            },
            None => PreparedWorker::Primary,
        };
        Ok(PreparedDistributedRuntime { runtime: self, worker })
    }

    #[must_use]
    pub const fn is_replica(&self) -> bool {
        self.replica.is_some()
    }
}

impl PreparedDistributedRuntime {
    pub(crate) async fn ignite_consensus_with_lifecycle(
        &self,
        lifecycle: crate::lifecycle::Lifecycle,
    ) -> anyhow::Result<Option<Consensus>> {
        self.runtime.ignite_consensus_with_lifecycle(lifecycle).await
    }

    pub(crate) fn start_with_lifecycle(
        self,
        lifecycle: crate::lifecycle::Lifecycle,
    ) -> std::io::Result<Option<AvailabilityRuntime>> {
        match self.worker {
            PreparedWorker::Primary => Ok(None),
            PreparedWorker::Replica { replica, runtime } => runtime
                .start_replica_services_with_lifecycle(
                    *replica,
                    self.runtime.analytics_puller,
                    self.runtime.beacon,
                    lifecycle,
                )
                .map(Some),
        }
    }

    pub(crate) async fn shutdown(self) -> std::io::Result<()> {
        match self.worker {
            PreparedWorker::Primary => Ok(()),
            PreparedWorker::Replica { runtime, .. } => runtime.shutdown().await,
        }
    }
}

#[async_trait::async_trait]
impl peryx_ha::AvailabilityRuntime for DistributedRuntime {
    type Context = crate::DistributedPrepareContext;
    type Routes = Router;
    type PreparedHandle = crate::DistributedHandle;
    type Error = anyhow::Error;

    fn routes(&self) -> Self::Routes {
        self.routes.clone().merge(
            Router::new()
                .route("/+replication/v1/health", get(availability_health))
                .route("/+replication/v1/ready", get(availability_readiness))
                .with_state(self.availability.clone()),
        )
    }

    fn metrics(&self) -> Vec<Arc<dyn PrometheusSource>> {
        self.prometheus.clone()
    }

    async fn prepare(
        self,
        context: Self::Context,
    ) -> Result<peryx_ha::PreparedAvailability<Self::Routes, Self::PreparedHandle>, Self::Error> {
        crate::service_assembly::prepare_runtime(self, context)
    }
}

fn consensus_plan(config: &RuntimeConfig) -> anyhow::Result<Option<ConsensusPlan>> {
    if config.mode != DistributedMode::Ha {
        return Ok(None);
    }
    let Some(membership) = config.membership.as_ref() else {
        return Ok(None);
    };
    let identity = config
        .node_identity
        .as_deref()
        .context("an `ha` consensus roster needs a `node-identity` naming this node's own member entry")?;
    let local = membership
        .members
        .iter()
        .find(|member| member.node == identity)
        .with_context(|| format!("this node's identity {identity:?} is not a member of the roster"))?;
    let members = membership
        .members
        .iter()
        .map(|member| ConsensusMember {
            datacenter: member.datacenter.clone(),
            address: member.address.clone(),
        })
        .collect::<Vec<_>>();
    ConsensusPlan::new(
        local.datacenter.clone(),
        local.role == RuntimeMemberRole::Writer,
        &members,
        config.data_dir.join(LOG_STORE_SUBPATH),
        membership.group.clone(),
        config.role.token().to_owned(),
    )
    .map(Some)
}

#[cfg(test)]
#[path = "../tests/unit/runtime_tests.rs"]
mod tests;
