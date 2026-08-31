//! Guards the destinations an OIDC provider names for itself.
//!
//! Discovery may put the token and key endpoints on hosts other than the issuer, so those
//! destinations come from a remote document rather than from operator configuration. This transport
//! puts them behind the same outbound policy artifact upstreams use: the operator's own issuer hosts
//! and approved endpoint hosts may reach private address space, and every other destination must be
//! globally routable both as a URL literal and as the address the connection resolves to.

use std::time::Duration;

use async_trait::async_trait;
use peryx_identity::OidcHttpTransport;
use peryx_upstream::OutboundGuard;
use url::Url;

/// Executes OIDC backchannel requests under the outbound destination policy.
#[derive(Debug, Clone)]
pub struct GuardedOidcTransport {
    client: reqwest::Client,
    guard: OutboundGuard,
}

impl GuardedOidcTransport {
    /// Trusts the host of every operator-configured issuer in `issuers` alongside every entry of
    /// `trusted_endpoint_hosts`.
    ///
    /// # Errors
    /// Returns [`GuardedOidcTransportError::Issuer`] when an issuer is not a URL carrying a host,
    /// and [`GuardedOidcTransportError::Client`] when the HTTP client cannot be built.
    pub fn new<'a, I, H, S>(
        issuers: I,
        trusted_endpoint_hosts: H,
        request_timeout: Duration,
    ) -> Result<Self, GuardedOidcTransportError>
    where
        I: IntoIterator<Item = &'a str>,
        H: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        // Installation is process-wide, so another caller may have installed the provider first.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let mut hosts = issuers
            .into_iter()
            .map(|issuer| {
                Url::parse(issuer)
                    .ok()
                    .and_then(|url| url.host_str().map(str::to_owned))
                    .ok_or(GuardedOidcTransportError::Issuer)
            })
            .collect::<Result<Vec<_>, _>>()?;
        hosts.extend(trusted_endpoint_hosts.into_iter().map(|host| host.as_ref().to_owned()));
        let guard = OutboundGuard::for_hosts(hosts);
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(request_timeout)
            .dns_resolver(guard.clone())
            .build()
            .or(Err(GuardedOidcTransportError::Client))?;
        Ok(Self { client, guard })
    }
}

#[async_trait]
impl OidcHttpTransport for GuardedOidcTransport {
    fn client(&self) -> &reqwest::Client {
        &self.client
    }

    fn permits(&self, url: &Url) -> bool {
        self.guard.check_url(url).is_ok()
    }

    async fn execute(&self, request: reqwest::Request) -> Result<reqwest::Response, reqwest::Error> {
        self.client.execute(request).await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GuardedOidcTransportError {
    #[error("the configured OIDC issuer is not a URL carrying a host")]
    Issuer,
    #[error("the OIDC HTTP client could not be built")]
    Client,
}

#[cfg(test)]
#[path = "../tests/unit/oidc/tests.rs"]
mod tests;
