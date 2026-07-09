//! Shared application state and index routing.

mod app;
mod describe;
mod index;

pub use app::{AppState, Clock, RuntimeOptions};
pub use describe::{
    HostedDescription, IndexDescription, SecretDescription, UpstreamDescription, describe_index, describe_indexes,
};
pub use index::{Index, IndexKind, Role};
