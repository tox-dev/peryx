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
use crate::{ProcessRouteMethodNotAllowed, RouteDescriptor, RouteRateLimit};

/// Concurrent upstream fetches allowed per cached index; `0` (the default) means unlimited.
///
/// The limiter remains off until configured so cold request bursts are not throttled by default.
pub const DEFAULT_UPSTREAM_CONCURRENCY: usize = 0;

/// Bounds upstream queueing so a stalled fetch returns a retryable error.
const UPSTREAM_WAIT_TIMEOUT: Duration = Duration::from_secs(30);

pub type UpstreamPermit = Option<OwnedSemaphorePermit>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RouteClass {
    Listing,
    Metadata,
    Artifact,
    Upload,
    Admin,
    Authentication,
}

impl RouteClass {
    const ALL: [Self; 6] = [
        Self::Listing,
        Self::Metadata,
        Self::Artifact,
        Self::Upload,
        Self::Admin,
        Self::Authentication,
    ];
    const COUNT: u64 = 6;

    #[must_use]
    pub const fn all() -> [Self; 6] {
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
            Self::Authentication => "authentication",
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
    pub authentication: RouteLimit,
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
            authentication: RouteLimit::new(60, 60),
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
            authentication: RouteLimit::new(60, 60),
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
            RouteClass::Authentication => self.authentication,
        }
    }
}

/// A monotonic time source, measured as elapsed time since the limiter was built. Production reads a
/// process `Instant`; a test injects a hand-advanced source so window resets are exercised without a
/// real sleep.
type Clock = Arc<dyn Fn() -> Duration + Send + Sync>;

pub struct RateLimiter {
    config: RateLimitConfig,
    // A fixed seed makes the bucket probe sequence independent of the per-process `RandomState` seed, so
    // cachegrind benchmarks of rate-limited routes stop flip-flopping between two instruction counts. moka
    // absorbs collisions and the cap is bounded, so a fixed seed costs no rate-limit accuracy.
    buckets: Cache<BucketKey, Arc<Mutex<Window>>, BuildHasherDefault<DefaultHasher>>,
    principal_hasher: RandomState,
    allowed: RouteCounters,
    denied: RouteCounters,
    clock: Clock,
}

impl RateLimiter {
    #[must_use]
    pub fn new(config: RateLimitConfig) -> Self {
        let base = Instant::now();
        Self::with_clock(config, Arc::new(move || base.elapsed()))
    }

    /// Build a limiter over an injected [`Clock`], so a test drives window resets by advancing time
    /// instead of sleeping the wall clock.
    fn with_clock(config: RateLimitConfig, clock: Clock) -> Self {
        let capacity = config.max_clients.saturating_mul(RouteClass::COUNT).max(1);
        Self {
            config,
            buckets: Cache::builder()
                .max_capacity(capacity)
                .build_with_hasher(BuildHasherDefault::<DefaultHasher>::default()),
            principal_hasher: RandomState::new(),
            allowed: RouteCounters::default(),
            denied: RouteCounters::default(),
            clock,
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

        let now = (self.clock)();
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
            retry_after: bucket.reset_at.saturating_sub(now).as_secs().max(1),
        })
    }

    /// Charge one request in `class` to the client at `ip` and report whether it stays within the
    /// limit. This is the synchronous decision [`enforce`] makes per request, exposed so callers can
    /// exercise the limiter (in tests and benchmarks) without driving a full HTTP request through the
    /// async router, where scheduling jitter would swamp the limiter's own cost.
    #[must_use]
    pub fn check_client(&self, class: RouteClass, ip: IpAddr) -> bool {
        self.check(class, ActorKey::Ip(ip)).is_ok()
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
    authentication: AtomicU64,
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
            RouteClass::Authentication => &self.authentication,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct BucketKey {
    class: RouteClass,
    actor: ActorKey,
}

struct Window {
    reset_at: Duration,
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

enum ForwardedClient {
    Resolved(IpAddr),
    TrustedChain,
    Malformed,
}

#[derive(Debug)]
struct MalformedForwarded;

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

    /// Queue cold request bursts at the concurrency cap instead of failing immediately.
    ///
    /// # Errors
    /// Returns [`UpstreamLimited`] only when no slot frees within `UPSTREAM_WAIT_TIMEOUT`.
    ///
    /// # Panics
    /// Panics if the private semaphore is closed. [`UpstreamLimits`] never closes it.
    pub async fn acquire(&self, name: &str) -> Result<UpstreamPermit, UpstreamLimited> {
        let Some(limit) = self.entries.get(name) else {
            return Ok(None);
        };
        let Some(semaphore) = &limit.semaphore else {
            return Ok(None);
        };
        let Ok(permit) = tokio::time::timeout(UPSTREAM_WAIT_TIMEOUT, semaphore.clone().acquire_owned()).await else {
            limit.denied.fetch_add(1, Ordering::Relaxed);
            // The full horizon prevents immediate retries from re-saturating the limiter.
            let retry_after = UPSTREAM_WAIT_TIMEOUT.as_secs();
            tracing::info!(
                target: "peryx::security",
                security_event = true,
                event = "rate_limit",
                action = "upstream_fetch",
                result = "denied",
                index = name,
                retry_after,
                "upstream concurrency wait timed out"
            );
            return Err(UpstreamLimited { retry_after });
        };
        Ok(Some(permit.expect("upstream semaphore stays open")))
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

    #[must_use]
    pub fn totals(&self) -> UpstreamLimitTotals {
        self.entries
            .values()
            .fold(UpstreamLimitTotals::default(), |mut totals, limit| {
                totals.in_flight += limit.semaphore.as_ref().map_or(0, |semaphore| {
                    limit.max_concurrent.saturating_sub(semaphore.available_permits())
                });
                totals.denied += limit.denied.load(Ordering::Relaxed);
                totals
            })
    }
}

pub struct UpstreamLimitSnapshot {
    pub index: String,
    pub max_concurrent: usize,
    pub in_flight: usize,
    pub denied: u64,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct UpstreamLimitTotals {
    pub in_flight: usize,
    pub denied: u64,
}

#[derive(Debug)]
pub struct UpstreamLimited {
    pub retry_after: u64,
}

pub async fn enforce(State(state): State<Arc<AppState>>, request: axum::extract::Request, next: Next) -> Response {
    if request.extensions().get::<ProcessRouteMethodNotAllowed>().is_some() {
        return next.run(request).await;
    }
    let declared_class = match request
        .extensions()
        .get::<RouteDescriptor>()
        .map(|route| route.rate_limit())
    {
        Some(RouteRateLimit::Class(class)) => Some(class),
        Some(RouteRateLimit::Exempt) => return next.run(request).await,
        None => None,
    };
    let path = request.uri().path();
    let mut service_post_class = None;
    if declared_class.is_none() && *request.method() == Method::POST {
        for (_, service) in state.driver_set().services() {
            if let Some(class) = service.classify_service_post(path.trim_start_matches('/'), request.headers()) {
                service_post_class = Some(class);
                break;
            }
        }
    }
    let class = declared_class
        .or(service_post_class)
        .or_else(|| ecosystem_route_class(request.method(), path));
    let has_authorization = request.headers().contains_key(header::AUTHORIZATION);
    // Avoid a second route lookup when credential validation and read classification both need the driver.
    let resolved_driver =
        if declared_class.is_none() && service_post_class.is_none() && (class.is_none() || has_authorization) {
            route_driver(&state, path)
        } else {
            None
        };
    let class = match (class, resolved_driver) {
        (Some(class), _) => class,
        (None, Some((driver, _))) => state
            .protocol_for(&driver.ecosystem())
            .map_or(RouteClass::Listing, |protocol| protocol.classify_route(path)),
        (None, None) => RouteClass::Listing,
    };
    let principal = if has_authorization
        && let Some((driver, position)) = resolved_driver
        && let Some(principal) = state.rate_limit_principal_for(&driver.ecosystem())
    {
        principal.resolve(&state.serving, position, request.headers())
    } else {
        peryx_identity::Principal::Anonymous
    };
    // Reject malformed forwarded identities because peer bucketing would merge distinct clients.
    let Ok(actor) = state.serving.rate_limits.actor_key(principal, &request) else {
        tracing::info!(
            target: "peryx::security",
            security_event = true,
            event = "rate_limit",
            action = "http_request",
            result = "rejected",
            reason = "malformed_forwarded_header",
            "rejected request with malformed forwarded header"
        );
        return malformed_forwarded_response();
    };
    match state.serving.rate_limits.check(class, actor) {
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

/// Classifies fallback routes shared by ecosystem namespaces.
///
/// The owning driver's `classify_route` handles ecosystem URL semantics, including top-level and
/// per-index namespaces.
#[must_use]
pub fn ecosystem_route_class(method: &Method, path: &str) -> Option<RouteClass> {
    let path = path.trim_start_matches('/');
    // HEAD and OPTIONS share read budgets because they do not mutate state.
    if matches!(*method, Method::POST | Method::PUT | Method::PATCH | Method::DELETE) {
        return Some(RouteClass::Upload);
    }
    if method != Method::GET && method != Method::HEAD && method != Method::OPTIONS {
        return Some(RouteClass::Upload);
    }
    if matches!(path, "stats" | "admin/status") || path.ends_with("/+api") || path.contains("/+api/") {
        return Some(RouteClass::Admin);
    }
    None
}

fn route_driver<'a>(
    state: &'a AppState,
    path: &str,
) -> Option<(&'a dyn crate::serving::EcosystemDriver, Option<usize>)> {
    if let Some(driver) = state.absolute_driver_for_path(path) {
        return Some((driver.as_ref(), None));
    }
    let (position, _) = state.serving.resolve_position(path.trim_start_matches('/'))?;
    Some((
        state.driver_for(&state.serving.index_at(position).ecosystem)?.as_ref(),
        Some(position),
    ))
}

impl RateLimiter {
    fn actor_key(
        &self,
        principal: peryx_identity::Principal,
        request: &axum::extract::Request,
    ) -> Result<ActorKey, MalformedForwarded> {
        match principal {
            peryx_identity::Principal::Named { subject } => {
                Ok(ActorKey::Token(self.principal_hasher.hash_one(subject)))
            }
            peryx_identity::Principal::Anonymous => Ok(ActorKey::Ip(
                self.client_ip(request)?.unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            )),
        }
    }

    /// Rejects malformed forwarded identities instead of merging clients into the peer bucket.
    fn client_ip(&self, request: &axum::extract::Request) -> Result<Option<IpAddr>, MalformedForwarded> {
        let Some(peer) = request.extensions().get::<ConnectInfo<SocketAddr>>() else {
            return Ok(None);
        };
        let peer = peer.0.ip().to_canonical();
        if !self.trusts_proxy(peer) {
            return Ok(Some(peer));
        }
        match self.forwarded_client_ip(request.headers()) {
            ForwardedClient::Resolved(client) => Ok(Some(client)),
            ForwardedClient::TrustedChain => Ok(Some(peer)),
            ForwardedClient::Malformed => Err(MalformedForwarded),
        }
    }

    fn forwarded_client_ip(&self, headers: &HeaderMap) -> ForwardedClient {
        let forwarded_values = headers.get_all("x-forwarded-for");
        if forwarded_values.iter().next().is_none() {
            return real_ip(headers);
        }

        // Scan the chain left to right, letting each hop overwrite the verdict. An untrusted address
        // becomes the client, a malformed token discards any client found so far (a proxy closer to us
        // than the last usable hop lied), and a trusted address leaves the verdict untouched. The final
        // state reflects the rightmost hop that mattered: a resolved client, a fully trusted chain, or a
        // malformed suffix that cannot share the peer bucket.
        let mut client = ForwardedClient::TrustedChain;
        for forwarded_value in forwarded_values {
            let Ok(forwarded_value) = forwarded_value.to_str() else {
                client = ForwardedClient::Malformed;
                continue;
            };
            for part in forwarded_value.split(',') {
                let Ok(address) = part.trim().parse::<IpAddr>().map(|address| address.to_canonical()) else {
                    client = ForwardedClient::Malformed;
                    continue;
                };
                if !self.trusts_proxy(address) {
                    client = ForwardedClient::Resolved(address);
                }
            }
        }
        client
    }
}

fn real_ip(headers: &HeaderMap) -> ForwardedClient {
    let mut real_values = headers.get_all("x-real-ip").iter();
    let Some(real_value) = real_values.next() else {
        return ForwardedClient::TrustedChain;
    };
    // Reject ambiguous identities because peer bucketing would merge distinct clients.
    if real_values.next().is_some() {
        return ForwardedClient::Malformed;
    }
    let Ok(real_value) = real_value.to_str() else {
        return ForwardedClient::Malformed;
    };
    real_value
        .trim()
        .parse::<IpAddr>()
        .map_or(ForwardedClient::Malformed, |address| {
            ForwardedClient::Resolved(address.to_canonical())
        })
}

fn malformed_forwarded_response() -> Response {
    (StatusCode::BAD_REQUEST, "malformed forwarded header").into_response()
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
#[path = "../tests/unit/rate_limit/tests.rs"]
mod tests;
