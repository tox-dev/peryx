//! `GET /+api` describes the running server: its service endpoints and one entry per configured index.
//!
//! The envelope (version, service URLs) and the public-base-URL resolution are ecosystem-agnostic and
//! live here; each ecosystem renders its per-index entry through its registered
//! [`ClientDiscovery`](crate::serving::ClientDiscovery) capability.

use std::str::FromStr as _;

use axum::http::{HeaderMap, Uri, header};
use peryx_core::url_encoding::push_component;
use serde_json::{Value, json};

/// The public base URL a client reaches this server at, used to render absolute URLs in discovery
/// entries. Resolved from the request (forwarded headers, then `Host`) or parsed from configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseUrl {
    origin: String,
    prefix: String,
}

#[derive(Debug, thiserror::Error)]
pub enum BaseUrlError {
    #[error("base URL must be an absolute http or https URL without credentials, query, or fragment")]
    Invalid,
}

impl BaseUrl {
    /// # Errors
    /// Returns [`BaseUrlError::Invalid`] unless the URL is absolute HTTP(S) without credentials,
    /// query, or fragment.
    pub fn parse(text: &str) -> Result<Self, BaseUrlError> {
        let parsed = url::Url::parse(text).map_err(|_| BaseUrlError::Invalid)?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.host_str().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(BaseUrlError::Invalid);
        }
        Ok(Self {
            origin: parsed.origin().ascii_serialization(),
            prefix: parsed.path().trim_end_matches('/').to_owned(),
        })
    }

    #[must_use]
    pub fn from_request(headers: &HeaderMap, uri: &Uri, trusted_proxy: bool) -> Option<Self> {
        let authority = uri
            .authority()
            .map(axum::http::uri::Authority::as_str)
            .or_else(|| {
                if trusted_proxy {
                    header_first(headers, "x-forwarded-host")
                } else {
                    None
                }
            })
            .or_else(|| header_one(headers, header::HOST))?;
        let scheme = uri
            .scheme_str()
            .or_else(|| {
                if trusted_proxy {
                    header_first(headers, "x-forwarded-proto")
                } else {
                    None
                }
            })
            .unwrap_or("http");
        Self::from_parts(scheme, authority).ok()
    }

    #[must_use]
    pub fn join(&self, path: &str) -> String {
        let mut url = String::with_capacity(self.origin.len() + self.prefix.len() + path.len());
        url.push_str(&self.origin);
        url.push_str(&self.prefix);
        url.push_str(path);
        url
    }

    /// The `host[:port]` a client dials, without the scheme. Clients name a
    /// registry by authority rather than URL.
    #[must_use]
    pub fn host_port(&self) -> &str {
        self.origin.split("://").nth(1).unwrap_or(&self.origin)
    }

    fn from_parts(scheme: &str, authority: &str) -> Result<Self, BaseUrlError> {
        let scheme = if scheme.eq_ignore_ascii_case("https") {
            "https"
        } else if scheme.eq_ignore_ascii_case("http") {
            "http"
        } else {
            return Err(BaseUrlError::Invalid);
        };
        let authority = axum::http::uri::Authority::from_str(authority).map_err(|_| BaseUrlError::Invalid)?;
        if authority.as_str().contains('@') {
            return Err(BaseUrlError::Invalid);
        }
        Self::parse(&format!("{scheme}://{authority}/"))
    }
}

#[must_use]
pub fn link(base: Option<&BaseUrl>, path: &str) -> String {
    base.map_or_else(|| path.to_owned(), |base| base.join(path))
}

#[must_use]
pub fn browse_path(route: &str) -> String {
    query_path("/browse", route)
}

#[must_use]
pub fn stats_path(route: &str) -> String {
    query_path("/stats", route)
}

fn query_path(prefix: &str, route: &str) -> String {
    let mut path = prefix.to_owned();
    path.push_str("?index=");
    push_component(&mut path, route);
    path
}

/// The `GET /+api` entry a driver with no richer rendering falls back to: the index's identity, without
/// the wire-protocol URLs or client setup a configured driver would add.
#[must_use]
pub fn minimal_entry(index: &crate::state::IndexDescription) -> Value {
    json!({
        "name": index.name,
        "route": index.route,
        "kind": index.kind,
        "ecosystem": index.ecosystem,
    })
}

#[must_use]
pub fn root_envelope(base: Option<&BaseUrl>, indexes: Vec<Value>) -> Value {
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "urls": service_urls(base),
        "indexes": Value::Array(indexes),
    })
}

#[must_use]
pub fn index_envelope(index: Value) -> Value {
    let mut document = serde_json::Map::new();
    document.insert("version".to_owned(), Value::from(env!("CARGO_PKG_VERSION")));
    document.insert("index".to_owned(), index);
    Value::Object(document)
}

fn service_urls(base: Option<&BaseUrl>) -> Value {
    json!({
        "api": link(base, "/+api"),
        "health": link(base, "/+health"),
        "readiness": link(base, "/+ready"),
        "status": link(base, "/+status"),
        "stats": link(base, "/+stats"),
        "openapi": link(base, "/api-docs/openapi.json"),
        "web": link(base, "/"),
    })
}

fn header_first<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    header_one(headers, name)?
        .split(',')
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn header_one<K>(headers: &HeaderMap, name: K) -> Option<&str>
where
    K: axum::http::header::AsHeaderName,
{
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
#[path = "../tests/unit/discovery/tests.rs"]
mod tests;
