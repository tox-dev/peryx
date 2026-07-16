//! Shared application state and index routing.

mod app;
mod build;
mod caches;
mod describe;
mod registry;

pub use app::{AppState, Clock, PrometheusSource, ServingState};
pub use build::{DEFAULT_HOT_CACHE_BYTES, DEFAULT_MAX_STALE_SECS, DEFAULT_TOKEN_TTL_SECS, RuntimeOptions};
pub use describe::{
    HostedDescription, IndexDescription, MemberDescription, SecretDescription, UpstreamDescription, describe_index,
    describe_indexes,
};
pub use peryx_index::{Index, IndexKind};
