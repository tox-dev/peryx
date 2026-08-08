use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::http::Method;

use super::{RateLimitConfig, RateLimiter, RouteClass, RouteLimit, service_route_class};

#[test]
fn test_check_client_allows_within_limit_then_denies_per_client() {
    let limiter = RateLimiter::new(RateLimitConfig {
        listing: RouteLimit::new(2, 60),
        ..RateLimitConfig::enabled_defaults()
    });
    let client = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 7));
    assert!(limiter.check_client(RouteClass::Listing, client));
    assert!(limiter.check_client(RouteClass::Listing, client));
    assert!(!limiter.check_client(RouteClass::Listing, client));
    // A separate client keeps its own budget rather than inheriting the exhausted one.
    assert!(limiter.check_client(RouteClass::Listing, IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9))));
}

#[test]
fn test_the_window_resets_and_readmits_once_time_advances_past_it() {
    let millis = Arc::new(AtomicU64::new(0));
    let handle = Arc::clone(&millis);
    let limiter = RateLimiter::with_clock(
        RateLimitConfig {
            listing: RouteLimit::new(1, 1),
            ..RateLimitConfig::enabled_defaults()
        },
        Arc::new(move || Duration::from_millis(handle.load(Ordering::SeqCst))),
    );
    let client = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 7));
    assert!(
        limiter.check_client(RouteClass::Listing, client),
        "the first request in the window is admitted"
    );
    assert!(
        !limiter.check_client(RouteClass::Listing, client),
        "the second exhausts the one-per-window budget"
    );

    // Advance past the one-second window with the injected clock, no wall-clock sleep: the bucket
    // resets and admits a fresh request.
    millis.store(1_001, Ordering::SeqCst);
    assert!(
        limiter.check_client(RouteClass::Listing, client),
        "a window whose reset time has passed readmits the client"
    );
}

#[test]
fn test_service_route_class_handles_writes_and_service_routes() {
    assert_eq!(
        service_route_class(&Method::POST, "/alpha/simple/"),
        Some(RouteClass::Upload)
    );
    assert_eq!(service_route_class(&Method::GET, "/+status"), Some(RouteClass::Admin));
    assert_eq!(service_route_class(&Method::GET, "/+acl"), Some(RouteClass::Admin));
    assert_eq!(
        service_route_class(&Method::GET, "/+revocations"),
        Some(RouteClass::Admin)
    );
    assert_eq!(
        service_route_class(&Method::PUT, "/+revocations/sha256:digest"),
        Some(RouteClass::Admin)
    );
    assert_eq!(
        service_route_class(&Method::POST, "/+revocations/sha256:digest/lift"),
        Some(RouteClass::Admin)
    );
    assert_eq!(
        service_route_class(&Method::GET, "/alpha/hosted/+api"),
        Some(RouteClass::Admin)
    );
    assert_eq!(
        service_route_class(&Method::GET, "/alpha/files/abc/x.bin.metadata"),
        None
    );
}

#[test]
fn test_service_route_class_treats_head_and_options_as_reads() {
    // Clients inspect artifacts with HEAD and OPTIONS before reads; classing them as reads
    // defers to the driver's own route class instead of spending the strict upload budget.
    assert_eq!(
        service_route_class(&Method::HEAD, "/v2/hub/library/nginx/manifests/latest"),
        None
    );
    assert_eq!(service_route_class(&Method::OPTIONS, "/alpha/simple/flask/"), None);
    assert_eq!(service_route_class(&Method::HEAD, "/+status"), Some(RouteClass::Admin));
    for method in [Method::PUT, Method::PATCH, Method::DELETE] {
        assert_eq!(
            service_route_class(&method, "/v2/hub/app/blobs/uploads/1"),
            Some(RouteClass::Upload)
        );
    }
    // Any other method keeps the original strict-budget default rather than deferring to the driver.
    assert_eq!(
        service_route_class(&Method::TRACE, "/alpha/simple/"),
        Some(RouteClass::Upload)
    );
}
