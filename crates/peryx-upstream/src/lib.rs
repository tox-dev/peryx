pub mod client;
mod route;

pub use client::retry;
pub use client::{
    Auth, BoundedRead, CredentialError, CredentialFailure, CredentialIdentity, CredentialProvider,
    CredentialProviderId, CredentialRefresh, CredentialScope, CredentialSnapshot, ExecCredentialConfig,
    ExecCredentialConfigError, ExecCredentialProviderError, Netrc, NetrcError, OutboundGuard, RangeError, RangeSession,
    Reachability, UpstreamClient, UpstreamError, UpstreamTls, UpstreamTlsError, redact_url,
};
pub use route::{ArtifactClient, NamedUpstream, RouteError, UpstreamHealth, UpstreamRouter};

#[cfg(test)]
#[path = "../tests/unit/mod.rs"]
mod tests;
