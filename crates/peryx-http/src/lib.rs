//! Drivers depend on neutral contracts, never on the router that dispatches to them.

pub mod handlers;
mod request_stall;
pub use request_stall::{BodyFailure, Stalled};
mod response_framing;
pub mod response_security;
pub mod router;

pub use peryx_driver::state::{
    AppState, DEFAULT_HOT_CACHE_BYTES, DEFAULT_MAX_STALE_SECS, Index, IndexDescription, IndexKind, RuntimeOptions,
    describe_indexes,
};
pub use router::{router, router_with_services, router_with_ui, service_route_descriptors};

#[cfg(test)]
#[path = "../tests/unit/tests/mod.rs"]
mod tests;
