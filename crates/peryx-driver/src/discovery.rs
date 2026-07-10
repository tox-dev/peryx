//! Neutral discovery scaffolding shared by every ecosystem.
//!
//! `GET /+api` describes the running server: its service endpoints and one entry per configured index.
//! The envelope (version, service URLs) and the public-base-URL resolution are ecosystem-agnostic and
//! live here; each ecosystem renders its own per-index entry (the Simple-API setup for `PyPI`, the
//! `docker pull` setup for `OCI`) through
//! [`EcosystemServing::discover_index`](crate::serving::EcosystemServing::discover_index) and
//! [`NamespaceServing::discover_index`](crate::serving::NamespaceServing::discover_index).

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
    /// Parse the public base URL used for absolute discovery URLs.
    ///
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
    pub fn from_request(headers: &HeaderMap, uri: &Uri) -> Option<Self> {
        let authority = uri
            .authority()
            .map(axum::http::uri::Authority::as_str)
            .or_else(|| header_first(headers, "x-forwarded-host"))
            .or_else(|| header_one(headers, header::HOST))?;
        let scheme = uri
            .scheme_str()
            .or_else(|| header_first(headers, "x-forwarded-proto"))
            .unwrap_or("http");
        Self::from_parts(scheme, authority).ok()
    }

    /// Append `path` (an absolute `/…` path) to the base origin and prefix.
    #[must_use]
    pub fn join(&self, path: &str) -> String {
        let mut url = String::with_capacity(self.origin.len() + self.prefix.len() + path.len());
        url.push_str(&self.origin);
        url.push_str(&self.prefix);
        url.push_str(path);
        url
    }

    /// The `host[:port]` a registry client dials, without the scheme. `docker`/`podman` name a
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

/// Render `path` (an absolute `/…` path) against the base, or return it relative when no base is known.
#[must_use]
pub fn link(base: Option<&BaseUrl>, path: &str) -> String {
    base.map_or_else(|| path.to_owned(), |base| base.join(path))
}

/// The web dashboard path for one index (`/browse?index=<route>`).
#[must_use]
pub fn browse_path(route: &str) -> String {
    query_path("/browse", route)
}

/// The stats path for one index (`/stats?index=<route>`).
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

/// The full `GET /+api` document: version, service endpoints, and one entry per index.
#[must_use]
pub fn root_envelope(base: Option<&BaseUrl>, indexes: Vec<Value>) -> Value {
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "urls": service_urls(base),
        "indexes": Value::Array(indexes),
    })
}

/// The `GET /{index}/+api` document: version and the single index's entry.
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
mod tests {
    use axum::http::{HeaderMap, HeaderValue, Uri};
    use rstest::rstest;

    use super::{BaseUrl, browse_path};

    #[test]
    fn test_base_url_rejects_credentials_query_and_fragment() {
        for url in [
            "not a url",
            "file:///tmp/simple",
            "https://user@example.test/",
            "https://example.test/?x=1",
            "https://example.test/#frag",
        ] {
            assert!(BaseUrl::parse(url).is_err(), "{url}");
        }
    }

    #[test]
    fn test_base_url_from_request_uses_forwarded_origin() {
        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("internal.test"));
        headers.insert(
            "x-forwarded-host",
            HeaderValue::from_static("packages.example, proxy.local"),
        );
        headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        let base = BaseUrl::from_request(&headers, &Uri::from_static("/+api")).unwrap();
        assert_eq!(
            base.join("/root/pypi/simple/"),
            "https://packages.example/root/pypi/simple/"
        );
    }

    #[rstest]
    #[case::no_headers(None, None)]
    #[case::unsupported_scheme(Some("packages.example"), Some("ssh"))]
    #[case::credentials_in_host(Some("user@packages.example"), Some("https"))]
    #[case::invalid_host(Some("packages example"), Some("https"))]
    fn test_base_url_from_request_rejects_invalid_forwarded_origin(
        #[case] host: Option<&str>,
        #[case] proto: Option<&str>,
    ) {
        let mut headers = HeaderMap::new();
        if let Some(host) = host {
            headers.insert("x-forwarded-host", HeaderValue::from_str(host).unwrap());
        }
        if let Some(proto) = proto {
            headers.insert("x-forwarded-proto", HeaderValue::from_str(proto).unwrap());
        }
        assert!(BaseUrl::from_request(&headers, &Uri::from_static("/+api")).is_none());
    }

    #[test]
    fn test_base_url_from_request_uses_host_header_without_proxy_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("packages.example"));
        let base = BaseUrl::from_request(&headers, &Uri::from_static("/+api")).unwrap();
        assert_eq!(
            base.join("/root/pypi/simple/"),
            "http://packages.example/root/pypi/simple/"
        );
    }

    #[test]
    fn test_host_port_strips_scheme() {
        let base = BaseUrl::parse("https://registry.example:5000/cache/").unwrap();
        assert_eq!(base.host_port(), "registry.example:5000");
    }

    #[test]
    fn test_browse_url_percent_encodes_route_query() {
        assert_eq!(browse_path("root/pypi"), "/browse?index=root%2Fpypi");
    }
}
