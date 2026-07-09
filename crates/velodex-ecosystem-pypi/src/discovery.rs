//! `PyPI` API discovery and client-configuration snippets.

use std::fmt::Write as _;

use serde::{Serialize, Serializer};
use serde_json::json;
use velodex_format::url_encoding::push_path;

use velodex_http::discovery::{BaseUrl, browse_path, link, stats_path};
use velodex_http::state::IndexDescription;

const TOKEN_PLACEHOLDER: &str = "<upload-token>";

#[derive(Debug, Serialize)]
struct IndexDiscovery {
    name: String,
    route: String,
    ecosystem: &'static str,
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

/// The `GET /+api` entry for one `PyPI` index: its Simple-API endpoints, capabilities, and the
/// `pip`/`uv`/`twine` client configuration.
///
/// # Panics
/// Never in practice: the entry is a fixed struct of strings and booleans that always serializes.
#[must_use]
pub fn index_entry(index: IndexDescription, base: Option<&BaseUrl>) -> serde_json::Value {
    serde_json::to_value(IndexDiscovery::new(index, base)).expect("index discovery serializes")
}

#[must_use]
pub fn snippet_text(base: &BaseUrl, route: &str, uploads: bool, kind: SnippetKind) -> Option<String> {
    let simple = base.join(&simple_path(route));
    match kind {
        SnippetKind::PipConf => Some(format!("[global]\nindex-url = {simple}\n")),
        SnippetKind::UvToml => Some(uv_toml(&simple, uploads.then(|| base.join(&upload_path(route))))),
        SnippetKind::Pypirc => uploads.then(|| pypirc(&base.join(&upload_path(route)))),
    }
}

impl IndexDiscovery {
    fn new(index: IndexDescription, base: Option<&BaseUrl>) -> Self {
        let client_configuration = base.map(|base| ClientConfiguration::new(base, &index.route, index.uploads));
        Self {
            urls: IndexUrls::new(base, &index.route, index.uploads),
            name: index.name,
            route: index.route,
            ecosystem: index.ecosystem,
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
        "simple_api_version": crate::API_VERSION,
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

#[cfg(test)]
mod tests {
    use super::{BaseUrl, SnippetKind, snippet_text};

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
}
