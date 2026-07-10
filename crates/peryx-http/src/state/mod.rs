//! Shared application state and index routing.

mod app;
mod describe;
mod index;

pub use app::{AppState, Clock, DEFAULT_HOT_CACHE_BYTES, DEFAULT_MAX_STALE_SECS, RuntimeOptions};
pub use describe::{
    HostedDescription, IndexDescription, SecretDescription, UpstreamDescription, describe_index, describe_indexes,
};
pub use index::{Index, IndexKind};
