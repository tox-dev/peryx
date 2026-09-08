use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::routing::get;
use peryx_driver::rate_limit::RouteClass;
use peryx_driver::{AppState, RouteDescriptor, RouteMethod, RoutePosture, RouteRateLimit, RouteSet};
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;
use tower::ServiceExt as _;

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

/// Both shapes a route set is taken apart into carry the registration: the router serves what was
/// registered on it, and the descriptors list it. The empty case above holds for a set that dropped
/// its routes just as well as for one that never had any.
#[tokio::test]
async fn a_route_set_carries_its_registration_into_both_shapes() {
    let directory = tempfile::tempdir().unwrap();
    let state = Arc::new(AppState::new(
        MetaStore::open(directory.path().join("peryx.redb")).unwrap(),
        BlobStore::new(directory.path().join("blobs")),
        60,
        Vec::new(),
    ));
    let descriptor = RouteDescriptor::new(
        RouteMethod::Get,
        "/+probe",
        RoutePosture::Read,
        RouteRateLimit::Class(RouteClass::Admin),
    );
    let registered = || RouteSet::default().route(descriptor, get(|| async { "probe" }));

    let (router, descriptors) = registered().into_parts();

    assert_eq!(descriptors, vec![descriptor]);
    assert_eq!(probe(router, &state).await, StatusCode::OK);
    assert_eq!(probe(registered().into_router(), &state).await, StatusCode::OK);
}

async fn probe(router: Router<Arc<AppState>>, state: &Arc<AppState>) -> StatusCode {
    router
        .with_state(state.clone())
        .oneshot(Request::builder().uri("/+probe").body(Body::empty()).unwrap())
        .await
        .unwrap()
        .status()
}
