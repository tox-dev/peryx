//! HTTP dispatch stays above this crate so ecosystem implementations depend only on neutral
//! contracts and process state.

pub mod access;
pub mod authz;
pub mod availability;
mod blocking_scan;
pub mod body;
pub mod client_address;
pub mod conditional;
pub mod discovery;
pub mod download;
mod driver_set;
mod http_routes;
pub mod http_services;
pub mod jobs;
pub mod oidc;
pub mod openapi;
pub mod quota;
pub mod range;
pub mod rate_limit;
pub mod retention;
pub mod revocations;
pub mod serving;
pub mod state;
pub mod tokens;
pub mod trash;
pub mod users;

pub use availability::{
    ServingStateAvailabilityAuthorizer, ServingStateControlAuthorizer, ServingStateMetadataFrontierProvider,
};
pub use blocking_scan::{BlockingScanExecutor, ScanCancellation};
pub use driver_set::{BlobReferenceScan, BlobReferenceScanError, DriverSet};
pub use http_routes::{
    HttpRoutes, MountedRoutes, ProcessRouteMethodNotAllowed, RouteDescriptor, RouteMethod, RoutePosture,
    RoutePrincipal, RouteRateLimit, RouteSet,
};
pub use serving::PolicyDryRunDriver;
pub use state::{
    AppState, DEFAULT_HOT_CACHE_BYTES, DEFAULT_MAX_STALE_SECS, Index, IndexDescription, IndexKind, PrometheusSource,
    ServingState,
};

#[must_use]
pub fn not_found() -> axum::response::Response {
    use axum::response::IntoResponse as _;
    (axum::http::StatusCode::NOT_FOUND, "not found").into_response()
}

#[cfg(test)]
#[path = "../tests/unit/tests/mod.rs"]
mod tests;
