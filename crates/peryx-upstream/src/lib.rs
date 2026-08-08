//! The upstream index client: fetch and conditionally revalidate simple pages and files from a
//! HTTP transport for ecosystem-owned upstream clients.

pub mod client;
mod route;

pub use client::retry;
pub use client::{
    Auth, CredentialError, CredentialFailure, CredentialIdentity, CredentialProvider, CredentialProviderId,
    CredentialRefresh, CredentialScope, CredentialSnapshot, ExecCredentialConfig, ExecCredentialConfigError,
    ExecCredentialProviderError, FileHead, Netrc, NetrcError, RangeError, Reachability, UpstreamClient, UpstreamError,
    UpstreamTls, UpstreamTlsError, redact_url,
};
pub use route::{ArtifactClient, NamedUpstream, RouteError, UpstreamHealth, UpstreamRouter};

#[cfg(test)]
#[path = "../tests/unit/tests/mod.rs"]
mod tests;
