//! API discovery and client configuration snippets.

use std::fmt::Write as _;
use std::str::FromStr as _;

use axum::http::{HeaderMap, Uri, header};
use serde::{Serialize, Serializer};
use serde_json::json;
use velodex_format::url_encoding::{push_component, push_path};

use crate::state::{AppState, IndexDescription};

const TOKEN_PLACEHOLDER: &str = "<upload-token>";

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
    /// Parse the public base URL used for absolute snippet URLs.
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
            .map(http::uri::Authority::as_str)
            .or_else(|| header_first(headers, "x-forwarded-host"))
            .or_else(|| header_one(headers, header::HOST))?;
        let scheme = uri
            .scheme_str()
            .or_else(|| header_first(headers, "x-forwarded-proto"))
            .unwrap_or("http");
        Self::from_parts(scheme, authority).ok()
    }

    #[must_use]
    fn join(&self, path: &str) -> String {
        let mut url = String::with_capacity(self.origin.len() + self.prefix.len() + path.len());
        url.push_str(&self.origin);
        url.push_str(&self.prefix);
        url.push_str(path);
        url
    }

    fn from_parts(scheme: &str, authority: &str) -> Result<Self, BaseUrlError> {
        let scheme = if scheme.eq_ignore_ascii_case("https") {
            "https"
        } else if scheme.eq_ignore_ascii_case("http") {
            "http"
        } else {
            return Err(BaseUrlError::Invalid);
        };
        let authority = http::uri::Authority::from_str(authority).map_err(|_| BaseUrlError::Invalid)?;
        if authority.as_str().contains('@') {
            return Err(BaseUrlError::Invalid);
        }
        Self::parse(&format!("{scheme}://{authority}/"))
    }
}

#[derive(Debug, Serialize)]
pub struct DiscoveryDocument {
    version: &'static str,
    urls: ServiceUrls,
    indexes: Vec<IndexDiscovery>,
}

#[derive(Debug, Serialize)]
pub struct IndexDocument {
    version: &'static str,
    index: IndexDiscovery,
}

#[derive(Debug, Serialize)]
struct ServiceUrls {
    api: String,
    status: String,
    stats: String,
    openapi: String,
    web: String,
}

#[derive(Debug, Serialize)]
struct IndexDiscovery {
    name: String,
    route: String,
    kind: &'static str,
    layers: Vec<String>,
    uploads: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    upload_to: Option<String>,
    #[serde(serialize_with = "serialize_capabilities")]
    capabilities: CapabilityFlags,
    urls: IndexUrls,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_configuration: Option<ClientConfiguration>,
}

#[derive(Debug)]
struct CapabilityFlags {
    writes: bool,
    volatile_deletes: bool,
}

#[derive(Debug, Serialize)]
struct IndexUrls {
    api: String,
    simple: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    upload: Option<String>,
    status: String,
    web: String,
    stats: String,
    openapi: String,
}

#[derive(Debug, Serialize)]
struct ClientConfiguration {
    #[serde(rename = "pip.conf")]
    pip_conf: String,
    #[serde(rename = "uv.toml")]
    uv_toml: String,
    #[serde(rename = ".pypirc", skip_serializing_if = "Option::is_none")]
    pypirc: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnippetKind {
    PipConf,
    UvToml,
    Pypirc,
}

#[must_use]
pub fn root_document(state: &AppState, base: Option<&BaseUrl>) -> DiscoveryDocument {
    DiscoveryDocument {
        version: env!("CARGO_PKG_VERSION"),
        urls: ServiceUrls::new(base),
        indexes: state
            .describe_indexes()
            .into_iter()
            .map(|index| IndexDiscovery::new(index, base))
            .collect(),
    }
}

#[must_use]
pub fn index_document(index: IndexDescription, base: Option<&BaseUrl>) -> IndexDocument {
    IndexDocument {
        version: env!("CARGO_PKG_VERSION"),
        index: IndexDiscovery::new(index, base),
    }
}

#[must_use]
pub fn snippet_text(base: &BaseUrl, route: &str, uploads: bool, kind: SnippetKind) -> Option<String> {
    let simple = absolute(base, &simple_path(route));
    match kind {
        SnippetKind::PipConf => Some(format!("[global]\nindex-url = {simple}\n")),
        SnippetKind::UvToml => Some(uv_toml(&simple, uploads.then(|| absolute(base, &upload_path(route))))),
        SnippetKind::Pypirc => uploads.then(|| pypirc(&absolute(base, &upload_path(route)))),
    }
}

impl ServiceUrls {
    fn new(base: Option<&BaseUrl>) -> Self {
        Self {
            api: link(base, "/+api"),
            status: link(base, "/+status"),
            stats: link(base, "/+stats"),
            openapi: link(base, "/api-docs/openapi.json"),
            web: link(base, "/"),
        }
    }
}

impl IndexDiscovery {
    fn new(index: IndexDescription, base: Option<&BaseUrl>) -> Self {
        let client_configuration = base.map(|base| ClientConfiguration::new(base, &index.route, index.uploads));
        Self {
            urls: IndexUrls::new(base, &index.route, index.uploads),
            name: index.name,
            route: index.route,
            kind: index.kind,
            layers: index.layers,
            uploads: index.uploads,
            upload_to: index.upload_to,
            capabilities: CapabilityFlags {
                writes: index.uploads,
                volatile_deletes: index.volatile_deletes,
            },
            client_configuration,
        }
    }
}

fn serialize_capabilities<S>(capabilities: &CapabilityFlags, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    json!({
        "simple_html": true,
        "simple_json": true,
        "simple_api_version": velodex_ecosystem_pypi::API_VERSION,
        "metadata_siblings": true,
        "uploads": capabilities.writes,
        "yanking": capabilities.writes,
        "volatile_deletes": capabilities.volatile_deletes,
        "project_status": true,
        "provenance": true,
        "legacy_json": true,
    })
    .serialize(serializer)
}

impl IndexUrls {
    fn new(base: Option<&BaseUrl>, route: &str, uploads: bool) -> Self {
        Self {
            api: link(base, &api_path(route)),
            simple: link(base, &simple_path(route)),
            upload: uploads.then(|| link(base, &upload_path(route))),
            status: link(base, "/+status"),
            web: link(base, &browse_path(route)),
            stats: link(base, &stats_path(route)),
            openapi: link(base, "/api-docs/openapi.json"),
        }
    }
}

impl ClientConfiguration {
    fn new(base: &BaseUrl, route: &str, uploads: bool) -> Self {
        Self {
            pip_conf: snippet_text(base, route, uploads, SnippetKind::PipConf).expect("pip.conf snippet exists"),
            uv_toml: snippet_text(base, route, uploads, SnippetKind::UvToml).expect("uv.toml snippet exists"),
            pypirc: snippet_text(base, route, uploads, SnippetKind::Pypirc),
        }
    }
}

fn uv_toml(simple: &str, upload: Option<String>) -> String {
    let mut text = String::new();
    if let Some(upload) = upload {
        let _ = write!(text, "publish-url = \"{upload}\"\n\n");
    }
    let _ = write!(
        text,
        "[[index]]\nname = \"velodex\"\nurl = \"{simple}\"\ndefault = true\n\n[pip]\nindex-url = \"{simple}\"\n"
    );
    text
}

fn pypirc(upload: &str) -> String {
    format!(
        "[distutils]\nindex-servers =\n    velodex\n\n[velodex]\nrepository = {upload}\nusername = __token__\npassword = {TOKEN_PLACEHOLDER}\n"
    )
}

fn link(base: Option<&BaseUrl>, path: &str) -> String {
    base.map_or_else(|| path.to_owned(), |base| base.join(path))
}

fn absolute(base: &BaseUrl, path: &str) -> String {
    base.join(path)
}

fn api_path(route: &str) -> String {
    let mut path = route_root(route);
    path.push_str("+api");
    path
}

fn simple_path(route: &str) -> String {
    let mut path = route_root(route);
    path.push_str("simple/");
    path
}

fn upload_path(route: &str) -> String {
    route_root(route)
}

fn route_root(route: &str) -> String {
    let mut path = String::with_capacity(route.len() + 2);
    path.push('/');
    push_path(&mut path, route);
    path.push('/');
    path
}

fn browse_path(route: &str) -> String {
    let mut path = "/browse".to_owned();
    QueryAppender::new(&mut path).push("index", route);
    path
}

fn stats_path(route: &str) -> String {
    let mut path = "/stats".to_owned();
    QueryAppender::new(&mut path).push("index", route);
    path
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

struct QueryAppender<'a> {
    path: &'a mut String,
    separator: char,
}

impl<'a> QueryAppender<'a> {
    const fn new(path: &'a mut String) -> Self {
        Self { path, separator: '?' }
    }

    fn push(&mut self, key: &str, value: &str) {
        self.path.push(self.separator);
        self.path.push_str(key);
        self.path.push('=');
        push_component(self.path, value);
        self.separator = '&';
    }
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue, Uri};

    use super::{BaseUrl, SnippetKind, browse_path, snippet_text};

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

    #[test]
    fn test_base_url_from_request_rejects_invalid_forwarded_origin() {
        let mut headers = HeaderMap::new();
        assert!(BaseUrl::from_request(&headers, &Uri::from_static("/+api")).is_none());

        headers.insert("x-forwarded-host", HeaderValue::from_static("packages.example"));
        headers.insert("x-forwarded-proto", HeaderValue::from_static("ssh"));
        assert!(BaseUrl::from_request(&headers, &Uri::from_static("/+api")).is_none());

        headers.insert("x-forwarded-host", HeaderValue::from_static("user@packages.example"));
        headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        assert!(BaseUrl::from_request(&headers, &Uri::from_static("/+api")).is_none());

        headers.insert("x-forwarded-host", HeaderValue::from_static("packages example"));
        headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
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
    fn test_relative_links_are_used_without_base_url() {
        let urls = super::IndexUrls::new(None, "root/pypi", true);
        assert_eq!(urls.api, "/root/pypi/+api");
        assert_eq!(urls.simple, "/root/pypi/simple/");
        assert_eq!(urls.upload, Some("/root/pypi/".to_owned()));
        assert_eq!(urls.web, "/browse?index=root%2Fpypi");
    }

    #[test]
    fn test_snippets_use_absolute_urls_and_redact_token() {
        let base = BaseUrl::parse("https://packages.example/cache/").unwrap();
        let text = snippet_text(&base, "root/pypi", true, SnippetKind::Pypirc).unwrap();
        assert_eq!(
            text,
            "[distutils]\nindex-servers =\n    velodex\n\n[velodex]\nrepository = https://packages.example/cache/root/pypi/\nusername = __token__\npassword = <upload-token>\n"
        );
    }

    #[test]
    fn test_browse_url_percent_encodes_route_query() {
        assert_eq!(browse_path("root/pypi"), "/browse?index=root%2Fpypi");
    }
}
