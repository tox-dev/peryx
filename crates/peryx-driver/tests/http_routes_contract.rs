use axum::http::Method;
use peryx_driver::rate_limit::RouteClass;
use peryx_driver::{RouteDescriptor, RouteMethod, RoutePosture, RouteRateLimit, RouteSet};

#[test]
fn route_methods_match_supported_http_methods() {
    for (route_method, method) in [
        (RouteMethod::Delete, Method::DELETE),
        (RouteMethod::Get, Method::GET),
        (RouteMethod::Get, Method::HEAD),
        (RouteMethod::Post, Method::POST),
        (RouteMethod::Put, Method::PUT),
    ] {
        assert!(route_method.matches(&method));
    }
    assert!(!RouteMethod::Get.matches(&Method::POST));
}

#[test]
fn route_descriptor_preserves_its_registration_contract() {
    let descriptor = RouteDescriptor::new(
        RouteMethod::Post,
        "/+example",
        RoutePosture::Mutation,
        RouteRateLimit::Class(RouteClass::Admin),
    );

    assert_eq!(
        (
            descriptor.method(),
            descriptor.path(),
            descriptor.posture(),
            descriptor.rate_limit(),
        ),
        (
            RouteMethod::Post,
            "/+example",
            RoutePosture::Mutation,
            RouteRateLimit::Class(RouteClass::Admin),
        )
    );
}

#[test]
fn empty_route_set_contains_no_router_descriptors() {
    let (_, descriptors) = RouteSet::default().into_parts();
    assert!(descriptors.is_empty());
}
