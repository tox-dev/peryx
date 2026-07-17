//! The upstream index client: fetch and conditionally revalidate simple pages and files from a
//! PEP 503/691 index (pypi.org by default, or any configured upstream).

pub mod client;
mod route;

pub use client::retry;
pub use client::{
    Auth, FileHead, Netrc, NetrcError, RangeError, Reachability, UpstreamClient, UpstreamError, redact_url,
};
pub use route::{ArtifactClient, NamedUpstream, RouteError, UpstreamHealth, UpstreamRouter};

#[cfg(test)]
mod tests;
