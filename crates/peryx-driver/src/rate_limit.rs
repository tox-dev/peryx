//! Local abuse controls for one peryx process.

use std::collections::HashMap;
use std::collections::hash_map::{DefaultHasher, RandomState};
use std::hash::{BuildHasher as _, BuildHasherDefault};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse as _, Response};
use ipnet::IpNet;
use moka::sync::Cache;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::state::AppState;

/// Concurrent upstream fetches allowed per cached index; `0` (the default) means unlimited.
///
/// Like every other peryx control, the upstream limiter is off until configured. A `uv`/`pip`
/// install issues a burst of cold requests, so a default cap would throttle every zero-config install
/// for no reason. Operators fronting a fragile upstream can set a cap, and then excess requests wait
/// for a slot (see [`UpstreamLimits::acquire`]) rather than fail.
pub const DEFAULT_UPSTREAM_CONCURRENCY: usize = 0;

/// How long a request waits for an upstream slot before giving up. A cold burst drains in far less;
/// exceeding it means the upstream is genuinely stalled, so the request returns a retryable error
/// instead of holding the client forever.
const UPSTREAM_WAIT_TIMEOUT: Duration = Duration::from_secs(30);

pub type UpstreamPermit = Option<OwnedSemaphorePermit>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RouteClass {
    Listing,
    Metadata,
    Artifact,
    Upload,
    Admin,
}

impl RouteClass {
    const ALL: [Self; 5] = [Self::Listing, Self::Metadata, Self::Artifact, Self::Upload, Self::Admin];
    const COUNT: u64 = 5;

    #[must_use]
    pub const fn all() -> [Self; 5] {
        Self::ALL
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Listing => "listing",
            Self::Metadata => "metadata",
            Self::Artifact => "artifact",
            Self::Upload => "upload",
            Self::Admin => "admin",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteLimit {
    pub requests: u64,
    pub window_secs: u64,
}

impl RouteLimit {
    #[must_use]
    pub const fn new(requests: u64, window_secs: u64) -> Self {
        Self { requests, window_secs }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitConfig {
    pub enabled: bool,
    pub max_clients: u64,
    pub trusted_proxies: Vec<IpNet>,
    pub listing: RouteLimit,
    pub metadata: RouteLimit,
    pub artifact: RouteLimit,
    pub upload: RouteLimit,
    pub admin: RouteLimit,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_clients: 8192,
            trusted_proxies: Vec::new(),
            listing: RouteLimit::new(600, 60),
            metadata: RouteLimit::new(1200, 60),
            artifact: RouteLimit::new(300, 60),
            upload: RouteLimit::new(60, 60),
            admin: RouteLimit::new(120, 60),
        }
    }
}

impl RateLimitConfig {
    #[must_use]
    pub const fn enabled_defaults() -> Self {
        Self {
            enabled: true,
            max_clients: 8192,
            trusted_proxies: Vec::new(),
            listing: RouteLimit::new(600, 60),
            metadata: RouteLimit::new(1200, 60),
            artifact: RouteLimit::new(300, 60),
            upload: RouteLimit::new(60, 60),
            admin: RouteLimit::new(120, 60),
        }
    }

    #[must_use]
    pub const fn limit(&self, class: RouteClass) -> RouteLimit {
        match class {
            RouteClass::Listing => self.listing,
            RouteClass::Metadata => self.metadata,
            RouteClass::Artifact => self.artifact,
            RouteClass::Upload => self.upload,
            RouteClass::Admin => self.admin,
        }
    }
}

pub struct RateLimiter {
    config: RateLimitConfig,
    // A fixed seed makes the bucket probe sequence independent of the per-process `RandomState` seed, so
    // cachegrind benchmarks of rate-limited routes stop flip-flopping between two instruction counts. moka
    // absorbs collisions and the cap is bounded, so a fixed seed costs no rate-limit accuracy.
    buckets: Cache<BucketKey, Arc<Mutex<Window>>, BuildHasherDefault<DefaultHasher>>,
    principal_hasher: RandomState,
    allowed: RouteCounters,
    denied: RouteCounters,
}

impl RateLimiter {
    #[must_use]
    pub fn new(config: RateLimitConfig) -> Self {
        let capacity = config.max_clients.saturating_mul(RouteClass::COUNT).max(1);
        Self {
            config,
            buckets: Cache::builder()
                .max_capacity(capacity)
                .build_with_hasher(BuildHasherDefault::<DefaultHasher>::default()),
            principal_hasher: RandomState::new(),
            allowed: RouteCounters::default(),
            denied: RouteCounters::default(),
        }
    }

    #[must_use]
    pub fn counters(&self) -> Vec<RouteLimitSnapshot> {
        RouteClass::all()
            .into_iter()
            .map(|class| RouteLimitSnapshot {
                class: class.as_str(),
                allowed: self.allowed.get(class),
                denied: self.denied.get(class),
            })
            .collect()
    }

    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.config.enabled
    }

    #[must_use]
    pub fn trusts_proxy(&self, address: IpAddr) -> bool {
        let address = address.to_canonical();
        self.config
            .trusted_proxies
            .iter()
            .any(|network| network.contains(&address))
    }

    fn check(&self, class: RouteClass, actor: ActorKey) -> Result<(), Limited> {
        let limit = self.config.limit(class);
        if limit.requests == 0 || limit.window_secs == 0 {
            self.allowed.increment(class);
            return Ok(());
        }

        let now = Instant::now();
        let window = Duration::from_secs(limit.window_secs);
        let bucket = self.buckets.get_with(BucketKey { class, actor }, || {
            Arc::new(Mutex::new(Window {
                reset_at: now + window,
                used: 0,
            }))
        });
        let mut bucket = bucket.lock().expect("rate limit bucket lock");
        if now >= bucket.reset_at {
            bucket.reset_at = now + window;
            bucket.used = 0;
        }
        if bucket.used < limit.requests {
            bucket.used += 1;
            self.allowed.increment(class);
            return Ok(());
        }
        self.denied.increment(class);
        Err(Limited {
            class,
            actor,
            retry_after: bucket.reset_at.saturating_duration_since(now).as_secs().max(1),
        })
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new(RateLimitConfig::default())
    }
}

pub struct RouteLimitSnapshot {
    pub class: &'static str,
    pub allowed: u64,
    pub denied: u64,
}

#[derive(Default)]
struct RouteCounters {
    listing: AtomicU64,
    metadata: AtomicU64,
    artifact: AtomicU64,
    upload: AtomicU64,
    admin: AtomicU64,
}

impl RouteCounters {
    fn increment(&self, class: RouteClass) {
        self.counter(class).fetch_add(1, Ordering::Relaxed);
    }

    fn get(&self, class: RouteClass) -> u64 {
        self.counter(class).load(Ordering::Relaxed)
    }

    const fn counter(&self, class: RouteClass) -> &AtomicU64 {
        match class {
            RouteClass::Listing => &self.listing,
            RouteClass::Metadata => &self.metadata,
            RouteClass::Artifact => &self.artifact,
            RouteClass::Upload => &self.upload,
            RouteClass::Admin => &self.admin,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct BucketKey {
    class: RouteClass,
    actor: ActorKey,
}

struct Window {
    reset_at: Instant,
    used: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ActorKey {
    Ip(IpAddr),
    Token(u64),
}

impl ActorKey {
    const fn kind(self) -> &'static str {
        match self {
            Self::Ip(_) => "ip",
            Self::Token(_) => "token",
        }
    }
}

struct Limited {
    class: RouteClass,
    actor: ActorKey,
    retry_after: u64,
}

#[derive(Default)]
pub struct UpstreamLimits {
    entries: HashMap<String, Arc<UpstreamLimit>>,
}

struct UpstreamLimit {
    max_concurrent: usize,
    semaphore: Option<Arc<Semaphore>>,
    denied: AtomicU64,
}

impl UpstreamLimits {
    #[must_use]
    pub fn new(limits: impl IntoIterator<Item = (String, usize)>) -> Self {
        Self {
            entries: limits
                .into_iter()
                .map(|(name, max_concurrent)| {
                    (
                        name,
                        Arc::new(UpstreamLimit {
                            max_concurrent,
                            semaphore: (max_concurrent > 0).then(|| Arc::new(Semaphore::new(max_concurrent))),
                            denied: AtomicU64::new(0),
                        }),
                    )
                })
                .collect(),
        }
    }

    /// Acquire one upstream slot for a cached index, waiting for a slot when the cap is reached.
    ///
    /// Back-pressure, not fast failure: a burst of cold requests (a `uv` install) queues at the
    /// concurrency cap and every request still succeeds, just serialized. Only a stall longer than
    /// [`UPSTREAM_WAIT_TIMEOUT`] gives up, and it does so with a retryable error rather than serving
    /// an empty page.
    ///
    /// # Errors
    /// Returns [`UpstreamLimited`] only when no slot frees within [`UPSTREAM_WAIT_TIMEOUT`].
    pub async fn acquire(&self, name: &str) -> Result<UpstreamPermit, UpstreamLimited> {
        let Some(limit) = self.entries.get(name) else {
            return Ok(None);
        };
        let Some(semaphore) = &limit.semaphore else {
            return Ok(None);
        };
        // The semaphore is never closed, so an inner acquire error is unreachable; reaching the `else`
        // means the deadline elapsed with no free slot.
        if let Ok(Ok(permit)) = tokio::time::timeout(UPSTREAM_WAIT_TIMEOUT, semaphore.clone().acquire_owned()).await {
            Ok(Some(permit))
        } else {
            limit.denied.fetch_add(1, Ordering::Relaxed);
            tracing::info!(
                target: "peryx::security",
                security_event = true,
                event = "rate_limit",
                action = "upstream_fetch",
                result = "denied",
                index = name,
                retry_after = 1_u64,
                "upstream concurrency wait timed out"
            );
            Err(UpstreamLimited { retry_after: 1 })
        }
    }

    #[must_use]
    pub fn snapshots(&self) -> Vec<UpstreamLimitSnapshot> {
        let mut snapshots: Vec<_> = self
            .entries
            .iter()
            .map(|(index, limit)| {
                let in_flight = limit.semaphore.as_ref().map_or(0, |semaphore| {
                    limit.max_concurrent.saturating_sub(semaphore.available_permits())
                });
                UpstreamLimitSnapshot {
                    index: index.clone(),
                    max_concurrent: limit.max_concurrent,
                    in_flight,
                    denied: limit.denied.load(Ordering::Relaxed),
                }
            })
            .collect();
        snapshots.sort_by(|left, right| left.index.cmp(&right.index));
        snapshots
    }
}

pub struct UpstreamLimitSnapshot {
    pub index: String,
    pub max_concurrent: usize,
    pub in_flight: usize,
    pub denied: u64,
}

#[derive(Debug)]
pub struct UpstreamLimited {
    pub retry_after: u64,
}

pub async fn enforce(State(state): State<Arc<AppState>>, request: axum::extract::Request, next: Next) -> Response {
    let path = request.uri().path();
    let class = service_route_class(request.method(), path);
    let has_authorization = request.headers().contains_key(header::AUTHORIZATION);
    // Avoid a second route lookup when credential validation and read classification both need the driver.
    let resolved_driver = if class.is_none() || has_authorization {
        route_driver(&state, path)
    } else {
        None
    };
    let class =
        class.unwrap_or_else(|| resolved_driver.map_or(RouteClass::Listing, |(driver, _)| driver.classify_route(path)));
    let principal = if has_authorization && let Some((driver, position)) = resolved_driver {
        driver.rate_limit_principal(&state, position, request.headers())
    } else {
        peryx_identity::Principal::Anonymous
    };
    let actor = state.rate_limits.actor_key(principal, &request);
    match state.rate_limits.check(class, actor) {
        Ok(()) => next.run(request).await,
        Err(limited) => {
            // Compute the log fields before the macro: as macro arguments they would evaluate only when
            // the callsite is enabled, so a run without a security-log subscriber would never cover them.
            let class = limited.class.as_str();
            let client = limited.actor.kind();
            tracing::info!(
                target: "peryx::security",
                security_event = true,
                event = "rate_limit",
                action = "http_request",
                result = "denied",
                class,
                client,
                retry_after = limited.retry_after,
                "request rate limit denied"
            );
            limited_response(limited.retry_after)
        }
    }
}

/// Classify the ecosystem-neutral part of a request: writes and peryx's own service endpoints.
///
/// Returns `None` for a GET inside an index's namespace (a project listing, metadata sibling, or
/// artifact), whose class depends on the ecosystem's URL scheme and so is decided by the owning
/// driver's `classify_route`: an indexed-mount driver's `classify_route`
/// for a per-index ecosystem, [`EcosystemDriver`](crate::serving::EcosystemDriver::classify_route)
/// for one that owns a top-level namespace.
#[must_use]
pub fn service_route_class(method: &Method, path: &str) -> Option<RouteClass> {
    // HEAD and OPTIONS are reads (an OCI client HEADs every manifest and blob before a pull); only a
    // body-bearing write method is an upload. Classifying them here as reads lets the owning driver's
    // `classify_route` bucket them with GET instead of spending the strict upload budget.
    if matches!(*method, Method::POST | Method::PUT | Method::PATCH | Method::DELETE) {
        return Some(RouteClass::Upload);
    }
    if method != Method::GET && method != Method::HEAD && method != Method::OPTIONS {
        return Some(RouteClass::Upload);
    }
    let path = path.trim_start_matches('/');
    if matches!(
        path,
        "" | "+api" | "+api/" | "+status" | "+acl" | "+stats" | "metrics" | "api-docs/openapi.json"
    ) || matches!(path, "stats" | "admin/status")
        || path.ends_with("/+api")
        || path.contains("/+api/")
    {
        return Some(RouteClass::Admin);
    }
    None
}

fn route_driver<'a>(
    state: &'a AppState,
    path: &str,
) -> Option<(&'a Arc<dyn crate::serving::EcosystemDriver>, Option<usize>)> {
    if let Some(driver) = state.absolute_driver_for_path(path) {
        return Some((driver, None));
    }
    let (position, _) = state.resolve_position(path.trim_start_matches('/'))?;
    Some((state.driver_for(state.index_at(position).ecosystem)?, Some(position)))
}

impl RateLimiter {
    fn actor_key(&self, principal: peryx_identity::Principal, request: &axum::extract::Request) -> ActorKey {
        match principal {
            peryx_identity::Principal::Named { subject } => ActorKey::Token(self.principal_hasher.hash_one(subject)),
            peryx_identity::Principal::Anonymous => {
                ActorKey::Ip(self.client_ip(request).unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST)))
            }
        }
    }

    fn client_ip(&self, request: &axum::extract::Request) -> Option<IpAddr> {
        let peer = request
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()?
            .0
            .ip()
            .to_canonical();
        if !self.trusts_proxy(peer) {
            return Some(peer);
        }
        Some(self.forwarded_client_ip(request.headers()).unwrap_or(peer))
    }

    fn forwarded_client_ip(&self, headers: &HeaderMap) -> Option<IpAddr> {
        let forwarded_values = headers.get_all("x-forwarded-for");
        if forwarded_values.iter().next().is_none() {
            let mut real_values = headers.get_all("x-real-ip").iter();
            let real_value = real_values.next()?.to_str().ok()?;
            if real_values.next().is_some() {
                return None;
            }
            return real_value
                .trim()
                .parse::<IpAddr>()
                .map(|address| address.to_canonical())
                .ok();
        }

        let mut client = None;
        let mut suffix_malformed = false;
        for forwarded_value in forwarded_values {
            let Ok(forwarded_value) = forwarded_value.to_str() else {
                client = None;
                suffix_malformed = true;
                continue;
            };
            for part in forwarded_value.split(',') {
                let Ok(address) = part.trim().parse::<IpAddr>().map(|address| address.to_canonical()) else {
                    client = None;
                    suffix_malformed = true;
                    continue;
                };
                if !self.trusts_proxy(address) {
                    client = Some(address);
                    suffix_malformed = false;
                }
            }
        }
        if suffix_malformed { None } else { client }
    }
}

fn limited_response(retry_after: u64) -> Response {
    let mut response = (StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded").into_response();
    response.headers_mut().insert(
        header::RETRY_AFTER,
        HeaderValue::from_str(&retry_after.to_string()).expect("integer retry-after is a valid header"),
    );
    response
}

#[cfg(test)]
mod tests {
    use axum::http::Method;

    use super::{RouteClass, service_route_class};

    #[test]
    fn test_service_route_class_handles_writes_and_service_routes() {
        assert_eq!(
            service_route_class(&Method::POST, "/pypi/simple/"),
            Some(RouteClass::Upload)
        );
        assert_eq!(service_route_class(&Method::GET, "/+status"), Some(RouteClass::Admin));
        assert_eq!(service_route_class(&Method::GET, "/+acl"), Some(RouteClass::Admin));
        assert_eq!(
            service_route_class(&Method::GET, "/pypi/hosted/+api"),
            Some(RouteClass::Admin)
        );
        assert_eq!(
            service_route_class(&Method::GET, "/pypi/files/abc/x.whl.metadata"),
            None
        );
    }

    #[test]
    fn test_service_route_class_treats_head_and_options_as_reads() {
        // An OCI client HEADs every manifest and blob before a pull; classing HEAD/OPTIONS as reads
        // defers to the driver's own route class instead of spending the strict upload budget.
        assert_eq!(
            service_route_class(&Method::HEAD, "/v2/hub/library/nginx/manifests/latest"),
            None
        );
        assert_eq!(service_route_class(&Method::OPTIONS, "/pypi/simple/flask/"), None);
        assert_eq!(service_route_class(&Method::HEAD, "/+status"), Some(RouteClass::Admin));
        for method in [Method::PUT, Method::PATCH, Method::DELETE] {
            assert_eq!(
                service_route_class(&method, "/v2/hub/app/blobs/uploads/1"),
                Some(RouteClass::Upload)
            );
        }
        // Any other method keeps the original strict-budget default rather than deferring to the driver.
        assert_eq!(
            service_route_class(&Method::TRACE, "/pypi/simple/"),
            Some(RouteClass::Upload)
        );
    }
}
