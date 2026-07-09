//! axum request handlers.
//!
//! All index traffic arrives on a catch-all path that is resolved to a configured index by longest
//! route prefix, then handed to that index's ecosystem serving driver. The handlers here are
//! ecosystem-neutral: they dispatch to the driver and serve the cross-cutting endpoints (search,
//! status, stats, metrics, `OpenAPI`, discovery).

mod discover;
mod dispatch;
mod query;
mod status;
mod usage;

pub use discover::{api, openapi_spec};
pub use dispatch::{dispatch_delete, dispatch_get, dispatch_post, dispatch_put, not_found};
pub use query::{search, search_error_response, search_response, search_response_offloaded};
pub use status::{StatusQuery, status};
pub use usage::{StatsQuery, ecosystem_summaries, family_descriptors, metrics, stats};
