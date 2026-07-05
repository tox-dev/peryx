//! Local abuse controls for one velodex process.

use std::collections::HashMap;
use std::hash::{Hash as _, Hasher as _};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse as _, Response};
use moka::sync::Cache;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::state::AppState;

pub const DEFAULT_UPSTREAM_CONCURRENCY: usize = 8;

pub type UpstreamPermit = Option<OwnedSemaphorePermit>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RouteClass {
    Simple,
    Metadata,
    Artifact,
    Upload,
    Admin,
}

impl RouteClass {
    const ALL: [Self; 5] = [Self::Simple, Self::Metadata, Self::Artifact, Self::Upload, Self::Admin];
    const COUNT: u64 = 5;

    #[must_use]
    pub const fn all() -> [Self; 5] {
        Self::ALL
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Simple => "simple",
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
    pub simple: RouteLimit,
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
            simple: RouteLimit::new(600, 60),
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
            simple: RouteLimit::new(600, 60),
            metadata: RouteLimit::new(1200, 60),
            artifact: RouteLimit::new(300, 60),
            upload: RouteLimit::new(60, 60),
            admin: RouteLimit::new(120, 60),
        }
    }

    #[must_use]
    pub const fn limit(&self, class: RouteClass) -> RouteLimit {
        match class {
            RouteClass::Simple => self.simple,
            RouteClass::Metadata => self.metadata,
            RouteClass::Artifact => self.artifact,
            RouteClass::Upload => self.upload,
            RouteClass::Admin => self.admin,
        }
    }
}

pub struct RateLimiter {
    config: RateLimitConfig,
    buckets: Cache<BucketKey, Arc<Mutex<Window>>>,
    allowed: RouteCounters,
    denied: RouteCounters,
}

impl RateLimiter {
    #[must_use]
    pub fn new(config: RateLimitConfig) -> Self {
        let capacity = config.max_clients.saturating_mul(RouteClass::COUNT).max(1);
        Self {
            config,
            buckets: Cache::builder().max_capacity(capacity).build(),
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
    simple: AtomicU64,
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
            RouteClass::Simple => &self.simple,
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

    /// Acquire one upstream slot for a mirror.
    ///
    /// # Errors
    /// Returns [`UpstreamLimited`] when the mirror has a concurrency cap and every slot is held.
    pub fn acquire(&self, name: &str) -> Result<UpstreamPermit, UpstreamLimited> {
        let Some(limit) = self.entries.get(name) else {
            return Ok(None);
        };
        let Some(semaphore) = &limit.semaphore else {
            return Ok(None);
        };
        semaphore.clone().try_acquire_owned().map(Some).map_err(|_| {
            limit.denied.fetch_add(1, Ordering::Relaxed);
            tracing::info!(
                target: "velodex::security",
                security_event = true,
                event = "rate_limit",
                action = "upstream_fetch",
                result = "denied",
                repository = name,
                retry_after = 1_u64,
                "upstream concurrency limit denied"
            );
            UpstreamLimited { retry_after: 1 }
        })
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

pub struct UpstreamLimited {
    pub retry_after: u64,
}

pub(crate) async fn enforce(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let class = route_class(request.method(), request.uri().path());
    let actor = actor_key(&request);
    match state.rate_limits.check(class, actor) {
        Ok(()) => next.run(request).await,
        Err(limited) => {
            let class = limited.class.as_str();
            let client = limited.actor.kind();
            tracing::info!(
                target: "velodex::security",
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

pub(crate) fn route_class(method: &Method, path: &str) -> RouteClass {
    if method != Method::GET {
        return RouteClass::Upload;
    }
    let path = path.trim_start_matches('/');
    if matches!(
        path,
        "" | "+api" | "+api/" | "+status" | "+stats" | "metrics" | "api-docs/openapi.json"
    ) || matches!(path, "stats" | "admin/status")
        || path.ends_with("/+api")
        || path.contains("/+api/")
    {
        return RouteClass::Admin;
    }
    if path.contains("/files/") && path.ends_with(".metadata") {
        return RouteClass::Metadata;
    }
    if path.contains("/files/") || path.contains("/inspect/") {
        return RouteClass::Artifact;
    }
    RouteClass::Simple
}

fn actor_key(request: &axum::extract::Request) -> ActorKey {
    if let Some(value) = request.headers().get(header::AUTHORIZATION) {
        return ActorKey::Token(header_hash(value));
    }
    ActorKey::Ip(peer_ip(request).unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST)))
}

fn header_hash(value: &HeaderValue) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.as_bytes().hash(&mut hasher);
    hasher.finish()
}

fn peer_ip(request: &axum::extract::Request) -> Option<IpAddr> {
    request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip())
        .or_else(|| forwarded_ip(request.headers()))
}

fn forwarded_ip(headers: &HeaderMap) -> Option<IpAddr> {
    headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').find_map(|part| part.trim().parse().ok()))
        .or_else(|| {
            headers
                .get("x-real-ip")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.trim().parse().ok())
        })
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

    use super::{RouteClass, route_class};

    #[test]
    fn test_route_class_detects_writes_and_metadata() {
        assert_eq!(route_class(&Method::POST, "/pypi/simple/"), RouteClass::Upload);
        assert_eq!(
            route_class(&Method::GET, "/pypi/files/abc/flask-1.0.whl.metadata"),
            RouteClass::Metadata
        );
    }
}
