use std::collections::HashMap;

use async_trait::async_trait;
use jsonwebtoken::DecodingKey;
use jsonwebtoken::jwk::{AlgorithmParameters, JwkSet, KeyOperations, PublicKeyUse};
use reqwest::header::{CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE, HeaderMap};
use url::Url;

pub const DISCOVERY_BODY_LIMIT: usize = 64 * 1024;
pub const JWKS_BODY_LIMIT: usize = 1024 * 1024;
pub const REFRESH_BACKOFF_SECS: i64 = 60;
pub const DEFAULT_FRESH_SECS: i64 = 300;
pub const MAX_FRESH_SECS: i64 = 900;
pub const HARD_CACHE_SECS: i64 = 3600;

/// The backchannel side of an OIDC provider: it shapes and sends requests, and it decides which
/// provider-declared destinations the deployment may connect to.
///
/// Discovery may name a token or key endpoint on any host, so those destinations arrive from a
/// remote document. Every implementation therefore answers [`permits`](Self::permits); there is no
/// default that would let a composition root fall back to an unchecked client.
#[async_trait]
pub trait OidcHttpTransport: Send + Sync {
    /// The client that shapes backchannel requests. Requests execute through
    /// [`execute`](Self::execute), so this client's own destination policy is the one that applies.
    fn client(&self) -> &reqwest::Client;

    /// Whether a provider-declared backchannel destination may be connected to.
    fn permits(&self, url: &Url) -> bool;

    /// Send a request without following redirects.
    async fn execute(&self, request: reqwest::Request) -> Result<reqwest::Response, reqwest::Error>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OidcHttpError {
    Unavailable,
    InvalidResponse,
}

pub async fn fetch(
    transport: &dyn OidcHttpTransport,
    request: reqwest::RequestBuilder,
    limit: usize,
) -> Result<(Vec<u8>, CachePolicy), OidcHttpError> {
    let response = fetch_bounded(transport, request, limit)
        .await
        .map_err(|error| match error {
            BoundedResponseError::Transport { .. } => OidcHttpError::Unavailable,
            BoundedResponseError::InvalidResponse { .. } => OidcHttpError::InvalidResponse,
        })?;
    if !response.status.is_success() || !response.json {
        return Err(OidcHttpError::InvalidResponse);
    }
    Ok((response.body, response.policy))
}

pub struct BoundedResponse {
    pub status: reqwest::StatusCode,
    pub json: bool,
    pub body: Vec<u8>,
    pub policy: CachePolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundedResponseError {
    Transport { status: Option<u16> },
    InvalidResponse { status: u16 },
}

pub async fn fetch_bounded(
    transport: &dyn OidcHttpTransport,
    request: reqwest::RequestBuilder,
    limit: usize,
) -> Result<BoundedResponse, BoundedResponseError> {
    let request = request
        .build()
        .map_err(|_| BoundedResponseError::Transport { status: None })?;
    let mut response = transport
        .execute(request)
        .await
        .map_err(|_| BoundedResponseError::Transport { status: None })?;
    let status = response.status();
    if response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .is_some_and(|length| length > limit)
    {
        return Err(BoundedResponseError::InvalidResponse {
            status: status.as_u16(),
        });
    }
    let json = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(is_json_content_type);
    let policy = cache_policy(response.headers());
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| BoundedResponseError::Transport {
        status: Some(status.as_u16()),
    })? {
        if body.len() + chunk.len() > limit {
            return Err(BoundedResponseError::InvalidResponse {
                status: status.as_u16(),
            });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(BoundedResponse {
        status,
        json,
        body,
        policy,
    })
}

pub fn discovery_url(issuer: &Url) -> Url {
    let mut discovery = issuer.clone();
    discovery.set_path(&format!(
        "{}/.well-known/openid-configuration",
        issuer.path().trim_end_matches('/')
    ));
    discovery
}

/// The storage and reuse rules [RFC 9111 section 5.2.2](https://www.rfc-editor.org/rfc/rfc9111.html#section-5.2.2)
/// lets a provider impose on one response.
pub struct CachePolicy {
    max_age: Option<i64>,
    storable: bool,
    validate: bool,
    forbid_stale: bool,
}

/// How long an entry cached at a given instant may answer, and whether it may be cached at all.
#[derive(Default)]
pub struct CacheWindow {
    /// Reuse without a successful revalidation ends here.
    pub fresh_until: i64,
    /// Reuse after a failed revalidation ends here.
    pub hard_until: i64,
    /// When false the response answers only the operation that fetched it.
    pub storable: bool,
}

impl CachePolicy {
    /// The stricter of the rules two documents impose on a cache entry built from both.
    pub fn strictest(&self, other: &Self) -> Self {
        Self {
            max_age: Some(self.age().min(other.age())),
            storable: self.storable && other.storable,
            validate: self.validate || other.validate,
            forbid_stale: self.forbid_stale || other.forbid_stale,
        }
    }

    /// The window an entry cached at `now` under these rules carries.
    pub fn window(&self, now: i64) -> CacheWindow {
        let fresh_until = now
            + if self.validate {
                0
            } else {
                self.age().min(MAX_FRESH_SECS)
            };
        CacheWindow {
            fresh_until,
            // `no-cache` and `must-revalidate` both forbid answering from a stale entry once validation fails.
            hard_until: if self.validate || self.forbid_stale {
                fresh_until
            } else {
                now + HARD_CACHE_SECS
            },
            storable: self.storable,
        }
    }

    fn age(&self) -> i64 {
        self.max_age.unwrap_or(DEFAULT_FRESH_SECS)
    }
}

/// Read the cache directives a provider sent. An unusable field line, a repeated `max-age`, and an
/// unparsable `max-age` all read as already stale, so a malformed header cannot extend reuse.
pub fn cache_policy(headers: &HeaderMap) -> CachePolicy {
    let mut policy = CachePolicy {
        max_age: None,
        storable: true,
        validate: false,
        forbid_stale: false,
    };
    for value in headers.get_all(CACHE_CONTROL) {
        let Some(directives) = value.to_str().ok().and_then(cache_directives) else {
            policy.validate = true;
            continue;
        };
        for directive in directives {
            let (name, argument) = directive
                .split_once('=')
                .map_or((directive, None), |(name, argument)| (name.trim_end(), Some(argument)));
            if name.eq_ignore_ascii_case("no-cache") {
                policy.validate = true;
            } else if name.eq_ignore_ascii_case("no-store") || name.eq_ignore_ascii_case("private") {
                policy.storable = false;
            } else if name.eq_ignore_ascii_case("must-revalidate") || name.eq_ignore_ascii_case("proxy-revalidate") {
                policy.forbid_stale = true;
            } else if name.eq_ignore_ascii_case("max-age") {
                policy.max_age = Some(match (policy.max_age, argument.and_then(delta_seconds)) {
                    (None, Some(seconds)) => seconds,
                    _ => 0,
                });
            }
        }
    }
    policy
}

pub fn usable_keys(jwks: JwkSet) -> Result<HashMap<String, DecodingKey>, OidcHttpError> {
    let mut keys = HashMap::new();
    for key in jwks.keys.into_iter().filter(|key| {
        matches!(key.algorithm, AlgorithmParameters::RSA(_))
            && key
                .common
                .key_algorithm
                .is_none_or(|algorithm| algorithm.to_string() == "RS256")
            && key
                .common
                .public_key_use
                .as_ref()
                .is_none_or(|usage| usage == &PublicKeyUse::Signature)
            && key
                .common
                .key_operations
                .as_ref()
                .is_none_or(|operations| operations.contains(&KeyOperations::Verify))
    }) {
        let Some(id) = key.common.key_id.as_deref().filter(|id| !id.is_empty()) else {
            continue;
        };
        let Ok(decoding_key) = DecodingKey::from_jwk(&key) else {
            continue;
        };
        if keys.insert(id.to_owned(), decoding_key).is_some() {
            return Err(OidcHttpError::InvalidResponse);
        }
    }
    if keys.is_empty() {
        return Err(OidcHttpError::InvalidResponse);
    }
    Ok(keys)
}

fn is_json_content_type(value: &str) -> bool {
    let media = value
        .split(';')
        .next()
        .map(str::trim)
        .unwrap_or_default()
        .to_ascii_lowercase();
    media == "application/json" || media.starts_with("application/") && media.ends_with("+json")
}

/// Split one field line on the commas outside a quoted string, so a qualified `no-cache="Set-Cookie"`
/// stays one directive. A quote that never closes leaves the line unusable.
fn cache_directives(value: &str) -> Option<Vec<&str>> {
    let mut directives = Vec::new();
    let mut quoted = false;
    let mut start = 0;
    for (index, byte) in value.bytes().enumerate() {
        if byte == b'"' {
            quoted = !quoted;
        } else if byte == b',' && !quoted {
            directives.push(value[start..index].trim());
            start = index + 1;
        }
    }
    if quoted {
        return None;
    }
    directives.push(value[start..].trim());
    Some(directives)
}

/// RFC 9111 `delta-seconds`, also accepting the quoted form legacy senders emit. A value past
/// [`i64::MAX`] saturates, which the freshness bound then caps anyway.
fn delta_seconds(value: &str) -> Option<i64> {
    let digits = value
        .strip_prefix('"')
        .map_or(Some(value), |quoted| quoted.strip_suffix('"'))?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some(digits.bytes().fold(0_i64, |seconds, byte| {
        seconds.saturating_mul(10).saturating_add(i64::from(byte - b'0'))
    }))
}

#[cfg(test)]
#[path = "../tests/unit/oidc_http/tests.rs"]
mod tests;
