//! Process-level replication configuration and follower scheduling.

mod raft;
pub use peryx_ha_distributed::AuthorityDrainJob;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse as _, Response};
use axum::routing::get;
use axum::{Json, Router};
use peryx_driver::AppState;
use peryx_ha_distributed::read_through::{
    DEFAULT_READ_THROUGH_LIMITS, DcTransport, ReadThroughLimits, RemotePlacementReader,
};
pub use peryx_ha_distributed::schedule_delay;
use peryx_ha_distributed::{
    AnalyticsPuller, AvailabilityMetrics, AvailabilityRuntime, BeaconSender, CapacityLimited, DEFAULT_BEACON_INTERVAL,
    DEFAULT_DEAD_AFTER, DEFAULT_RECONNECT_POLICY, DEFAULT_SET_LIMITS, DEFAULT_SUSPECT_AFTER, DEFAULT_TRANSFER_LIMITS,
    DurabilityPolicy, HttpBlobTransport, HttpPeerTransport, LivenessTracker, MemberFrontier, MemberRole, PeerSet,
    REPLICA_BLOB_FETCH_CONCURRENCY, ReplicaLoop, ReplicaLoopParts, ReplicaMonitor, ReplicaReclamationFrontiers,
    SetLimits, TransferLimits, WorkerShared, analytics_router, follower_router, group_readiness, liveness_router,
    primary_router, resolve_producer_epoch,
};
use peryx_http::handlers::status_authorization;
use peryx_http::response_security::{
    ClassifiedField, FieldClassification, ProtectedCachePolicy, ResponseAuthorization, filter_fields,
};
use peryx_storage::meta::DataCenterId;
use peryx_upstream::redact_url;
use serde_json::{Value, json};

use crate::config::{AvailabilityConfig, Config, DcMembership, DcRole, ReplicationConfig, SecretSource};

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

/// What a replica exposes about the primary it follows: the shared monitor and the primary's origin
/// with credentials, query, and fragment already removed.
#[derive(Clone)]
struct ReplicaView {
    monitor: Arc<ReplicaMonitor>,
    upstream: String,
}

/// The readiness reason a replica reports when its background worker domain has failed, or `None`
/// when no worker runs or the runtime is still healthy. A panicked task clears the domain's health,
/// so a replica keeps serving reads under its staleness contract while readiness names the fault.
fn worker_reason(workers: Option<&Arc<WorkerShared>>) -> Option<&'static str> {
    workers
        .filter(|workers| !workers.is_healthy())
        .map(|_| "worker_unhealthy")
}

/// The availability surface a `dc` or `ha` process serves its health and readiness resources from.
/// A `none` process holds none of this, so it mounts neither resource.
#[derive(Clone)]
struct AvailabilityNode {
    app: Arc<AppState>,
    mode: &'static str,
    role: AvailabilityRole,
    replica: Option<ReplicaView>,
    /// The writer's view of replica liveness, present only when a `dc`/`ha` primary follows a
    /// configured member roster. It informs routing hints and never gates this node's own readiness.
    liveness: Option<Arc<LivenessTracker>>,
    /// The configured roster and durability policy a writer folds member frontiers against to publish
    /// DC group readiness. Present only on a primary that names a member roster; a replica and a
    /// rosterless node publish no group readiness.
    group: Option<GroupReadinessSource>,
    workers: Option<Arc<WorkerShared>>,
}

/// The static inputs a writer needs to fold live member frontiers into DC group readiness: the full
/// configured roster with each member's role, and the durability policy the group acknowledges under.
#[derive(Clone)]
struct GroupReadinessSource {
    members: Vec<(String, MemberRole)>,
    policy: DurabilityPolicy,
}

/// The roster and durability policy for a `dc`/`ha` writer, or `None` when no member roster is
/// configured. The policy defaults to a strict majority of the configured members, the safe quorum for
/// a static group, until an operator selects another.
fn group_readiness_source(config: &Config) -> Option<GroupReadinessSource> {
    let members = config
        .dc_membership
        .as_ref()?
        .members
        .iter()
        .map(|member| {
            let role = match member.role {
                DcRole::Writer => MemberRole::Writer,
                DcRole::Replica => MemberRole::Replica,
            };
            (member.node.clone(), role)
        })
        .collect();
    Some(GroupReadinessSource {
        members,
        policy: DurabilityPolicy::Majority,
    })
}

impl AvailabilityNode {
    /// Whether this node can serve at its frontier, and every reason it cannot. The list is empty
    /// exactly when the node is ready, so a probe reads `ready` and a human reads the causes. The
    /// blob store carries a mount's crash and replication guarantees, so its reachability gates
    /// readiness; a replica's metadata frontier is validated at startup and tracked below.
    async fn readiness(&self) -> (bool, Vec<&'static str>) {
        let mut reasons = Vec::new();
        if self.app.blobs.health().await.is_err() {
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

    /// The readiness verdict and a body already filtered to the caller's class: mode, role, and the
    /// verdict for any caller; serials and lag for an operator; the redacted primary origin for an
    /// administrator.
    async fn document(&self, authorization: ResponseAuthorization) -> (bool, serde_json::Map<String, Value>) {
        let (ready, reasons) = self.readiness().await;
        let mut fields = vec![
            ClassifiedField::new("mode", FieldClassification::Public, json!(self.mode)),
            ClassifiedField::new("role", FieldClassification::Public, json!(self.role.as_str())),
            ClassifiedField::new("ready", FieldClassification::Public, json!(ready)),
            ClassifiedField::new("reasons", FieldClassification::Public, json!(reasons)),
        ];
        if let Some(replica) = &self.replica {
            let observation = replica.monitor.snapshot();
            let lag = observation
                .primary_serial
                .map(|primary_serial| primary_serial.saturating_sub(observation.serial));
            fields.extend([
                ClassifiedField::new("serial", FieldClassification::Operator, json!(observation.serial)),
                ClassifiedField::new(
                    "primary_serial",
                    FieldClassification::Operator,
                    json!(observation.primary_serial),
                ),
                ClassifiedField::new("lag", FieldClassification::Operator, json!(lag)),
                ClassifiedField::new(
                    "synced_changes",
                    FieldClassification::Operator,
                    json!(observation.changes),
                ),
                ClassifiedField::new("sync_errors", FieldClassification::Operator, json!(observation.errors)),
                ClassifiedField::new("upstream", FieldClassification::Administrator, json!(replica.upstream)),
            ]);
        } else {
            let serial = self.app.meta.current_serial().unwrap_or(0);
            fields.push(ClassifiedField::new(
                "serial",
                FieldClassification::Operator,
                json!(serial),
            ));
            let now = Instant::now();
            if let Some(liveness) = &self.liveness {
                fields.push(ClassifiedField::new(
                    "peers",
                    FieldClassification::Operator,
                    json!(liveness.summary(now)),
                ));
            }
            if let Some(group) = &self.group {
                fields.push(ClassifiedField::new(
                    "group_readiness",
                    FieldClassification::Operator,
                    self.group_readiness(group, serial, now),
                ));
            }
        }
        let body = filter_fields(authorization, fields).expect("public and allowed scopes classify");
        (ready, body)
    }

    /// Fold the configured roster's live frontiers into the group's readiness and durable frontier. The
    /// writer's own frontier is its committed `serial`; each replica's is the one it last reported over a
    /// beacon still within the dead window, or `None` when it has gone silent. A silent or lost member
    /// counts as not contributing, so the fixed roster size, never a shrunken one, measures the quorum.
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
            "policy": "majority",
            "blocked": readiness.blocked,
        })
    }
}

/// `GET /+replication/v1/health`: availability liveness. It answers `200` whenever the process still
/// serves the resource, so a load balancer never restarts a replica that is merely catching up. The
/// body carries the readiness verdict for callers that want one document.
async fn availability_health(State(node): State<AvailabilityNode>, headers: HeaderMap) -> Response {
    let authorization = status_authorization(&node.app, &headers).await;
    let (_ready, body) = node.document(authorization).await;
    availability_response(StatusCode::OK, body)
}

/// `GET /+replication/v1/ready`: availability readiness. It answers `503` for a frontier gap, an
/// incompatible primary schema, or a failed local store, so readiness removes the node from a pool
/// without restarting it.
async fn availability_readiness(State(node): State<AvailabilityNode>, headers: HeaderMap) -> Response {
    let authorization = status_authorization(&node.app, &headers).await;
    let (ready, body) = node.document(authorization).await;
    let status = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    availability_response(status, body)
}

fn availability_response(status: StatusCode, body: serde_json::Map<String, Value>) -> Response {
    let mut response = (status, Json(Value::Object(body))).into_response();
    ProtectedCachePolicy::NoStore.apply(response.headers_mut());
    response
}

/// How much of the primary's blob traffic one dual-plane fetch pass may hold in flight, and how long a
/// single blob request may run before it is a retryable loss.
const BLOB_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

const METADATA_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Install the serving read-through capability when this node knows its own datacenter and has peers to
/// fetch a missed public download from.
///
/// A node reaches a peer's blob-serving endpoint with the same replication token its metadata plane
/// uses, so the capability is available only under `dc`/`ha` with a configured roster. This node's own
/// datacenter comes from its writer identity; a node that does not carry one (a replica) installs no
/// capability yet and serves misses through its ecosystem's upstream path. Peers are keyed by the unique
/// datacenter each roster member runs in, the node's own excluded so it never fetches from itself.
///
/// # Errors
/// Returns an error if the replication token cannot be read or a peer address is not a usable HTTP base.
pub fn install_read_through(config: &Config, state: &Arc<AppState>) -> anyhow::Result<()> {
    let (Some(replication), Some(membership)) = (config.availability.replication(), config.dc_membership.as_ref())
    else {
        return Ok(());
    };
    let Some(local_dc) = local_datacenter(config, membership) else {
        return Ok(());
    };
    let (ReplicationConfig::Primary { token, .. } | ReplicationConfig::Replica { token, .. }) = replication;
    let token = token.read().context("read the replication token for read-through")?;
    let limits = config.read_through.unwrap_or(DEFAULT_READ_THROUGH_LIMITS);
    let built = peer_blob_delegates(membership, &local_dc, &token, limits)?;
    if built.is_empty() {
        return Ok(());
    }
    let delegates: HashMap<String, DcTransport> = built
        .into_iter()
        .map(|(dc, transport)| (dc, Arc::new(transport) as DcTransport))
        .collect();
    let dc = DataCenterId::new(local_dc.clone()).with_context(|| format!("local datacenter identity {local_dc}"))?;
    let reader = RemotePlacementReader::new(dc, delegates, limits, state.serving.clock.clone());
    state.set_read_through(Arc::new(reader));
    Ok(())
}

/// Build a bounded blob transport to every roster member outside `local_dc`, keyed by the peer datacenter
/// it reaches.
///
/// These are the peers a replica defers a blob to and read-through fetches a public miss from; the local
/// datacenter holds no delegate, since a local miss is exactly the reason to reach a peer. The transfer
/// bounds come from the read-through limits, so a ranged fetch stays under the same per-fetch ceiling. The
/// map is empty when the roster has no remote peer.
///
/// # Errors
/// Returns an error when a member address is not a usable HTTP base.
fn peer_blob_delegates(
    membership: &DcMembership,
    local_dc: &str,
    token: &str,
    limits: ReadThroughLimits,
) -> anyhow::Result<HashMap<String, CapacityLimited<HttpBlobTransport>>> {
    let transfer = TransferLimits {
        max_operations: TransferLimits::default().max_operations,
        max_encoded_bytes: limits.per_fetch_bytes,
    };
    let mut delegates = HashMap::new();
    for member in &membership.members {
        if member.dc == local_dc {
            continue;
        }
        let base = peer_blob_base(&member.address);
        let transport = HttpBlobTransport::new(&base, token.to_owned(), transfer, BLOB_FETCH_TIMEOUT)
            .with_context(|| format!("build a read-through blob transport for datacenter {}", member.dc))?;
        delegates.insert(member.dc.clone(), CapacityLimited::new(transport, limits.concurrency));
    }
    Ok(delegates)
}

/// Resolve a replica's own datacenter and the per-peer blob delegates its pull deferral consults.
///
/// A blob the policy places only on one of these reachable peer datacenters is deferred to cross-DC
/// read-through instead of whole-pulled; the delegate keys are exactly those peers. A replica that has not
/// resolved its own datacenter - no roster, or a node identity absent from it - resolves no peers and
/// whole-pulls every blob, the behavior before descriptors named where each blob lives.
///
/// The peer addresses were validated building the metadata peer set just before this, so rebuilding a
/// blob transport over the same address cannot fail.
fn replica_blob_deferral(
    config: &Config,
    token: &str,
) -> anyhow::Result<(String, HashMap<String, CapacityLimited<HttpBlobTransport>>)> {
    let local = config
        .dc_membership
        .as_ref()
        .and_then(|membership| local_datacenter(config, membership));
    let (Some(membership), Some(local_dc)) = (config.dc_membership.as_ref(), local) else {
        return Ok((String::new(), HashMap::new()));
    };
    let limits = config.read_through.unwrap_or(DEFAULT_READ_THROUGH_LIMITS);
    let delegates = peer_blob_delegates(membership, &local_dc, token, limits)?;
    Ok((local_dc, delegates))
}

/// A peer's blob-serving base URL from its roster address. A bare `host:port` reaches the peer over
/// plaintext HTTP, the availability plane's default; an operator terminating TLS gives a full
/// `https://` address, which is used as written.
fn peer_blob_base(address: &str) -> String {
    if address.starts_with("http://") || address.starts_with("https://") {
        address.to_owned()
    } else {
        format!("http://{address}")
    }
}

/// This node's own datacenter, read from the roster member its own identity names. A node names its own
/// entry through `node_identity`; `writer_identity` is the one writer every node claims and is the same
/// across the group, so it cannot tell one node's datacenter from another's. Falls back to
/// `writer_identity` for a single-writer group that configures no distinct node identity, and a node in
/// neither field has none.
fn local_datacenter(config: &Config, membership: &DcMembership) -> Option<String> {
    let identity = config.node_identity.as_deref().or(config.writer_identity.as_deref())?;
    membership
        .members
        .iter()
        .find(|member| member.node == identity)
        .map(|member| member.dc.clone())
}

const UPSTREAM_SOURCE: &str = "upstream";

/// Apply the effects of one replicated page: log it, rebuild the derived views the changed keys touch,
/// and advance the readable frontier over the applied serial once every required view reflects it.
///
/// Each ecosystem driver rebuilds its own affected views. When they all succeed the search view frontier
/// advances to `outcome.serial`, so a read the gate held becomes visible only after the view it depends
/// on caught up. When a driver reports a [`ViewBlock`](peryx_driver::state::ViewBlock) the frontier stays
/// where it was - the read stays held - and the lazy full refresh a later search runs recovers it. The
/// mutation-epoch bump keeps that recovery path armed for every ecosystem view. A page
/// with no changes does nothing. This is synchronous and independent of the async sync loop, so a direct
/// test covers it deterministically rather than riding on async scheduling.
#[cfg(test)]
pub(crate) fn apply_replicated_page(
    app: &AppState,
    outcome: peryx_ha_distributed::SyncOutcome,
    changed_keys: &[String],
) {
    peryx_ha::ReplicaViewApplier::apply(
        app,
        peryx_ha::ReplicaPage {
            changes: outcome.changes,
            serial: outcome.serial,
            primary_serial: outcome.primary_serial,
        },
        changed_keys,
    );
}

/// The delay the replica loop waits after one pass: nothing between pages that still have more to pull,
/// the poll interval once caught up, and a reconnect-policy backoff after a retryable transport loss,
/// falling back to the poll interval once the policy gives up, so a peer that never recovers keeps trying
/// at the base cadence rather than spinning. `attempt` counts consecutive transport losses and resets on
/// any pass that applied.
/// Returns an error when a member address is not a usable peer URL.
fn metadata_peers(
    membership: Option<&DcMembership>,
    this: Option<&str>,
    upstream: &str,
    token: &str,
    resume: u64,
    page_size: std::num::NonZeroUsize,
) -> anyhow::Result<PeerSet<HttpPeerTransport>> {
    // Fetch at the replica's configured page size, not the set default, so an operator's page bound
    // still governs how much each peer serves per round.
    let limits = SetLimits {
        request_size: page_size,
        ..DEFAULT_SET_LIMITS
    };
    let mut set = PeerSet::new(limits, DEFAULT_RECONNECT_POLICY);
    let mut joined = std::collections::BTreeSet::new();
    if let Some(membership) = membership {
        for member in &membership.members {
            if Some(member.node.as_str()) == this || !joined.insert(member.address.clone()) {
                continue;
            }
            let transport =
                HttpPeerTransport::new(&member.address, token, DEFAULT_TRANSFER_LIMITS, METADATA_FETCH_TIMEOUT)
                    .with_context(|| format!("build the metadata peer transport for {}", member.node))?;
            set.join(member.node.clone(), transport, resume);
        }
    }
    if joined.insert(upstream.to_owned()) {
        let transport = HttpPeerTransport::new(upstream, token, DEFAULT_TRANSFER_LIMITS, METADATA_FETCH_TIMEOUT)
            .context("build the upstream metadata transport")?;
        set.join(UPSTREAM_SOURCE, transport, resume);
    }
    Ok(set)
}

/// Build a replica's two data planes: the multi-peer metadata puller over the roster and the bounded
/// blob transport to its upstream.
///
/// # Errors
/// Returns an error when a peer address or the upstream URL is not a usable base.
fn replica_transports(
    config: &Config,
    upstream: &str,
    token: &str,
    resume: u64,
    page_size: std::num::NonZeroUsize,
) -> anyhow::Result<(PeerSet<HttpPeerTransport>, CapacityLimited<HttpBlobTransport>)> {
    let metadata = metadata_peers(
        config.dc_membership.as_ref(),
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

/// Track the configured replica members so the writer can age their beacons into routing hints. A
/// process without a member roster, or a roster naming no replica, tracks nothing.
/// Build a writer's replication router and availability node: the change feed, the analytics endpoint,
/// the liveness ingest for a configured roster, and the group-readiness source it publishes.
///
/// # Errors
/// Propagates a token read failure or a route-build failure.
fn build_primary(
    config: &Config,
    state: &Arc<AppState>,
    mode: &'static str,
    source: &str,
    token: &SecretSource,
) -> anyhow::Result<(Router, AvailabilityNode)> {
    let token = token.read().context("read the primary replication token")?;
    let router = primary_router(
        source.to_owned(),
        token.clone(),
        state.serving.meta.clone(),
        state.serving.blobs.clone(),
    )
    .context("build primary replication routes")?;
    let router = merge_analytics_endpoint(router, config, state, &token)?;
    let liveness = primary_liveness(config);
    let router = match &liveness {
        Some(tracker) => router.merge(liveness_router(token, tracker.clone()).context("build liveness ingest routes")?),
        None => router,
    };
    let node = AvailabilityNode {
        app: state.clone(),
        mode,
        role: AvailabilityRole::Primary,
        replica: None,
        liveness,
        group: group_readiness_source(config),
        workers: None,
    };
    Ok((router, node))
}

/// The replica's frontier beacon, or `None` when it names no `node_identity` for the writer to
/// recognize it by. The `upstream` and `token` are the ones the caller already validated building the
/// metadata transport, so the beacon reuses them without re-validating and its constructor cannot fail.
fn replica_beacon(config: &Config, state: &Arc<AppState>, upstream: &str, token: &str) -> Option<BeaconSender> {
    let node = config.node_identity.as_deref()?;
    Some(
        BeaconSender::new(
            upstream,
            token,
            node,
            u64::try_from((state.clock)()).unwrap_or(0),
            state.serving.meta.clone(),
            DEFAULT_BEACON_INTERVAL,
        )
        .expect("the validated upstream and token also build the frontier beacon"),
    )
}

fn primary_liveness(config: &Config) -> Option<Arc<LivenessTracker>> {
    let replicas: Vec<String> = config
        .dc_membership
        .as_ref()?
        .members
        .iter()
        .filter(|member| member.role == DcRole::Replica)
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

/// Replication routes and follower work prepared from one resolved configuration.
pub struct ReplicationRuntime {
    routes: Router,
    replica: Option<(ReplicaLoop, AvailabilityRuntime)>,
    availability: AvailabilityNode,
    /// The replica's analytics pull worker, spawned onto the availability runtime alongside the replica
    /// loop. `None` unless this process follows an upstream to pull analytics batches from.
    analytics_puller: Option<AnalyticsPuller>,
    /// The ownership consensus seed for an `ha` process, resolved synchronously here and ignited into a
    /// running node once the async runtime is up. `None` under `none` and `dc`, which run no group.
    consensus: Option<raft::ConsensusPlan>,
    /// The replica's frontier beacon to its writer, spawned alongside the replica loop. `None` unless
    /// this process follows an upstream and knows its own roster identity to report under.
    beacon: Option<BeaconSender>,
}

/// The running ownership consensus an `ha` process holds for its lifetime.
///
/// Carries the group handle the mutation path submits first-publish claims to, and the peer RPC router
/// the process mounts on its peer-facing listener so remote voters can exchange raft RPCs with this node.
/// Dropping it shuts the raft runtime down.
pub struct Consensus {
    /// The group handle to register on the [`AppState`] for the mutation path.
    pub authority: Arc<dyn peryx_driver::state::OwnershipAuthority>,
    /// The same group as the membership and transfer command seam the administrator control plane submits
    /// through, wrapped once the runtime holds a clock.
    pub control: Arc<dyn peryx_driver::state::MembershipControl>,
    /// The receive-side raft RPC routes to mount on the peer-facing (availability) listener.
    pub peer_router: Router,
}

/// Merge the producer's sealed-batch analytics endpoint into `router` when this node has an identity to
/// stamp its intervals with, leaving `router` untouched otherwise.
fn merge_analytics_endpoint(
    router: Router,
    config: &Config,
    state: &Arc<AppState>,
    token: &str,
) -> anyhow::Result<Router> {
    let Some(identity) = &config.node_identity else {
        return Ok(router);
    };
    let epoch =
        resolve_producer_epoch(&state.serving.meta.analytics()).context("resolve the analytics producer epoch")?;
    Ok(router.merge(analytics_router(
        token.to_owned(),
        state.serving.metrics.clone(),
        peryx_ha_distributed::ProducerId(identity.clone()),
        epoch,
    )))
}

/// Build the replica's background analytics pull worker, or `None` when this node is not a replica.
fn build_analytics_puller(config: &Config, state: &Arc<AppState>) -> anyhow::Result<Option<AnalyticsPuller>> {
    let Some(ReplicationConfig::Replica {
        upstream,
        token,
        poll_interval,
        ..
    }) = config.availability.replication()
    else {
        return Ok(None);
    };
    let token = token
        .read()
        .context("read the replica replication token for analytics")?;
    let puller = AnalyticsPuller::new(upstream, token, state.serving.meta.analytics(), *poll_interval)
        .context("build the analytics pull worker")?;
    Ok(Some(puller))
}

impl ReplicationRuntime {
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

    /// Return no runtime when distributed availability is disabled.
    ///
    /// # Errors
    /// Returns an error when the selected distributed runtime cannot be prepared.
    pub fn from_config(config: &Config, state: &Arc<AppState>) -> anyhow::Result<Option<Self>> {
        if matches!(config.availability, AvailabilityConfig::None) {
            return Ok(None);
        }
        Self::new(config, state).map(Some)
    }

    /// Prepare the configured replication role without starting background work.
    ///
    /// # Errors
    /// Returns an error if a secret cannot be read, the upstream URL is invalid, or the primary
    /// router rejects its identity or token.
    pub fn new(config: &Config, state: &Arc<AppState>) -> anyhow::Result<Self> {
        let (mode, replication) = match &config.availability {
            AvailabilityConfig::None => anyhow::bail!("distributed runtime requested while availability is disabled"),
            AvailabilityConfig::Dc(replication) => ("dc", replication),
            AvailabilityConfig::Ha(replication) => ("ha", replication),
        };
        let (routes, replica, availability, beacon) = match replication {
            ReplicationConfig::Primary { source, token } => {
                let (router, node) = build_primary(config, state, mode, source, token)?;
                (router, None, node, None)
            }
            ReplicationConfig::Replica {
                upstream,
                token,
                poll_interval,
                page_size,
            } => {
                let token = token.read().context("read the replica replication token")?;
                let resume = state.meta.current_serial().context("read the replica serial")?;
                let (metadata, transport) = replica_transports(config, upstream, &token, resume, *page_size)?;
                let (local_dc, delegates) = replica_blob_deferral(config, &token)?;
                let beacon = replica_beacon(config, state, upstream, &token);
                let follower = follower_router(token, state.serving.meta.clone())
                    .context("build the follower change-feed routes")?;
                let monitor = Arc::new(ReplicaMonitor::new(resume));
                let metrics = Arc::new(AvailabilityMetrics::default());
                let workers = Arc::new(WorkerShared::for_replica());
                state.register_prometheus(monitor.clone());
                state.register_prometheus(metrics.clone());
                state.register_prometheus(workers.clone());
                let runtime =
                    AvailabilityRuntime::start(workers.clone()).context("build the availability worker runtime")?;
                let node = AvailabilityNode {
                    app: state.clone(),
                    mode,
                    role: AvailabilityRole::Replica,
                    replica: Some(ReplicaView {
                        monitor: monitor.clone(),
                        upstream: redact_url(upstream),
                    }),
                    liveness: None,
                    group: None,
                    workers: Some(workers),
                };
                (
                    follower,
                    Some((
                        ReplicaLoop::new(ReplicaLoopParts {
                            views: state.clone(),
                            metadata,
                            policy: DEFAULT_RECONNECT_POLICY,
                            meta: state.serving.meta.clone(),
                            blobs: state.serving.blobs.clone(),
                            page_size: *page_size,
                            poll_interval: *poll_interval,
                            monitor,
                            metrics,
                            transport,
                            local_dc,
                            delegates,
                        }),
                        runtime,
                    )),
                    node,
                    beacon,
                )
            }
        };
        // A `dc` or `ha` node resolves each write to a datacenter durability decision, so it exposes the
        // outcome counters; single-node `none` runs no such decision and registers nothing.
        state.serving.configure_distributed();
        if let Some(metrics) = state.serving.dc_durability() {
            state.register_prometheus(metrics.clone());
        }
        Ok(Self {
            routes,
            replica,
            availability,
            analytics_puller: build_analytics_puller(config, state)?,
            consensus: raft::consensus_plan(config)?,
            beacon,
        })
    }

    /// Ignite the ownership consensus node, when this process runs an `ha` group, returning the running
    /// handle for the caller to hold for the process lifetime. A `none` or `dc` process runs no group and
    /// returns `None`. This is the async step the synchronous [`new`](Self::new) defers: it opens the
    /// durable log store and bootstraps the group.
    ///
    /// # Errors
    /// Returns an error when the consensus log store cannot be opened or the node cannot start or
    /// bootstrap.
    pub async fn ignite_consensus(&self) -> anyhow::Result<Option<Consensus>> {
        match &self.consensus {
            Some(plan) => {
                let node = plan.ignite().await?;
                let peer_router =
                    peryx_ha_distributed::raft::network::raft_rpc_router(plan.token(), node.rpc_handler())
                        .context("build the raft peer rpc router")?;
                let group = Arc::new(raft::OwnershipGroup::new(node, plan.home()));
                Ok(Some(Consensus {
                    authority: group.clone(),
                    control: group,
                    peer_router,
                }))
            }
            None => Ok(None),
        }
    }

    /// Whether this process follows a primary and must avoid local writers.
    #[must_use]
    pub const fn is_replica(&self) -> bool {
        self.replica.is_some()
    }

    /// Mount primary routes and the availability health and readiness resources, when configured, on
    /// the process router. A `none` process has no availability surface and mounts neither resource.
    pub fn mount(&self, router: Router) -> Router {
        router.merge(self.routes.clone()).merge(
            Router::new()
                .route("/+replication/v1/health", get(availability_health))
                .route("/+replication/v1/ready", get(availability_readiness))
                .with_state(self.availability.clone()),
        )
    }

    /// Start the replica loop on its own availability runtime, when configured, returning the
    /// runtime so the caller keeps it alive for the process lifetime. Follower work runs on the
    /// bounded worker pool rather than the foreground request executor.
    ///
    /// # Errors
    /// Returns an error when the availability runtime cannot reserve a configured service slot.
    pub fn start(self) -> anyhow::Result<Option<AvailabilityRuntime>> {
        let Some((replica, runtime)) = self.replica else {
            return Ok(None);
        };
        runtime
            .start_replica_services(replica, self.analytics_puller, self.beacon)
            .map(Some)
    }

    #[cfg(test)]
    pub(crate) async fn sync_cycle(&mut self) -> Option<bool> {
        let (replica, _) = self.replica.as_mut().expect("sync cycle requires a replica runtime");
        Some(replica.cycle().await.unwrap_or(true))
    }
}

#[cfg(test)]
#[path = "../tests/unit/replication/tests.rs"]
mod tests;
