use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use jsonwebtoken::DecodingKey;
use jsonwebtoken::jwk::{AlgorithmParameters, JwkSet, KeyOperations, PublicKeyUse};
use reqwest::header::{CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE};
use url::Url;

pub const DISCOVERY_BODY_LIMIT: usize = 64 * 1024;
pub const JWKS_BODY_LIMIT: usize = 1024 * 1024;
pub const MIN_FRESH_SECS: i64 = 60;
pub const DEFAULT_FRESH_SECS: i64 = 300;
pub const MAX_FRESH_SECS: i64 = 900;

#[async_trait]
pub trait OidcHttpTransport: Send + Sync {
    /// Send a request without following redirects.
    async fn execute(&self, request: reqwest::Request) -> Result<reqwest::Response, reqwest::Error>;
}

#[derive(Debug)]
pub struct ReqwestOidcHttpTransport(pub reqwest::Client);

#[async_trait]
impl OidcHttpTransport for ReqwestOidcHttpTransport {
    async fn execute(&self, request: reqwest::Request) -> Result<reqwest::Response, reqwest::Error> {
        self.0.execute(request).await
    }
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
) -> Result<(Vec<u8>, Option<i64>), OidcHttpError> {
    let response = fetch_bounded(transport, request, limit)
        .await
        .map_err(|error| match error {
            BoundedResponseError::Transport { .. } => OidcHttpError::Unavailable,
            BoundedResponseError::InvalidResponse { .. } => OidcHttpError::InvalidResponse,
        })?;
    if !response.status.is_success() || !response.json {
        return Err(OidcHttpError::InvalidResponse);
    }
    Ok((response.body, response.max_age))
}

pub struct BoundedResponse {
    pub status: reqwest::StatusCode,
    pub json: bool,
    pub body: Vec<u8>,
    pub max_age: Option<i64>,
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
    let max_age = response
        .headers()
        .get(CACHE_CONTROL)
        .and_then(|value| value.to_str().ok())
        .and_then(cache_max_age);
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
        max_age,
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

pub fn freshness(discovery: Option<i64>, jwks: Option<i64>) -> i64 {
    discovery
        .unwrap_or(DEFAULT_FRESH_SECS)
        .min(jwks.unwrap_or(DEFAULT_FRESH_SECS))
        .clamp(MIN_FRESH_SECS, MAX_FRESH_SECS)
}

pub fn usable_keys(jwks: JwkSet) -> Result<HashMap<String, DecodingKey>, OidcHttpError> {
    let mut ids = HashSet::new();
    if jwks.keys.is_empty()
        || jwks.keys.iter().any(|key| {
            let Some(id) = key.common.key_id.as_deref().filter(|id| !id.is_empty()) else {
                return true;
            };
            !ids.insert(id)
        })
    {
        return Err(OidcHttpError::InvalidResponse);
    }
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
        let id = key.common.key_id.clone().expect("key IDs were validated");
        keys.insert(
            id,
            DecodingKey::from_jwk(&key).map_err(|_| OidcHttpError::InvalidResponse)?,
        );
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

fn cache_max_age(value: &str) -> Option<i64> {
    value.split(',').find_map(|directive| {
        let (name, value) = directive.trim().split_once('=')?;
        name.eq_ignore_ascii_case("max-age")
            .then(|| value.trim_matches('"').parse().ok())
            .flatten()
    })
}
