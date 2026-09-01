//! Trusted client address attribution for HTTP requests.

use std::net::IpAddr;

use axum::extract::Request;
use axum::http::Extensions;

use crate::rate_limit::{MalformedForwarded, RateLimiter};

#[derive(Clone, Copy)]
struct ClientAddress(Result<Option<IpAddr>, MalformedForwarded>);

/// Caches the trusted-proxy decision before rate limiting and routing.
#[must_use]
pub fn attach(limits: &RateLimiter, mut request: Request) -> Request {
    let resolution = limits.client_ip(&request);
    request.extensions_mut().insert(ClientAddress(resolution));
    request
}

/// Returns the trusted client address, excluding missing and malformed identities.
#[must_use]
pub fn resolved(extensions: &Extensions) -> Option<IpAddr> {
    resolution(extensions)?.ok()?
}

pub(crate) fn resolution(extensions: &Extensions) -> Option<Result<Option<IpAddr>, MalformedForwarded>> {
    extensions.get::<ClientAddress>().map(|address| address.0)
}

#[cfg(test)]
#[path = "../tests/unit/client_address/tests.rs"]
mod tests;
