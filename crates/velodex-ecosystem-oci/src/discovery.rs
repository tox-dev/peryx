//! OCI API discovery and `docker`/`podman` client configuration.
//!
//! The `GET /+api` entry for an OCI index describes its distribution-spec `/v2/` endpoint, the
//! capabilities velodex serves for it, and a copyable `docker pull` (and, when the index accepts
//! writes, `docker push`) setup. The neutral discovery handler wraps this entry alongside every other
//! ecosystem's into one document.

use std::fmt::Write as _;

use serde_json::{Value, json};
use velodex_http::discovery::{BaseUrl, browse_path, link, stats_path};
use velodex_http::state::IndexDescription;

const IMAGE_PLACEHOLDER: &str = "<image>";
const TAG_PLACEHOLDER: &str = "<tag>";
const HOST_PLACEHOLDER: &str = "<host>";

/// The `GET /+api` entry for one `OCI` index.
#[must_use]
pub fn index_entry(index: IndexDescription, base: Option<&BaseUrl>) -> Value {
    let IndexDescription {
        name,
        route,
        ecosystem,
        kind,
        layers,
        uploads,
        volatile_deletes,
        ..
    } = index;
    let api = link(base, &format!("/{route}/+api"));
    let web = link(base, &browse_path(&route));
    let stats = link(base, &stats_path(&route));
    let docker = docker_snippet(base, &route, uploads);
    json!({
        "name": name,
        "route": route,
        "kind": kind,
        "ecosystem": ecosystem,
        "layers": layers,
        "uploads": uploads,
        "capabilities": {
            "distribution_v2": true,
            "manifest_pull": true,
            "blob_pull": true,
            "tags_list": true,
            "referrers": true,
            "layer_browser": true,
            "manifest_push": uploads,
            "volatile_deletes": volatile_deletes,
        },
        "urls": {
            "api": api,
            "registry": link(base, "/v2/"),
            "status": link(base, "/+status"),
            "web": web,
            "stats": stats,
            "openapi": link(base, "/api-docs/openapi.json"),
        },
        "client_configuration": {
            "docker": docker,
        },
    })
}

fn docker_snippet(base: Option<&BaseUrl>, route: &str, uploads: bool) -> String {
    let host = base.map_or(HOST_PLACEHOLDER, BaseUrl::host_port);
    let reference = format!("{host}/{route}/{IMAGE_PLACEHOLDER}:{TAG_PLACEHOLDER}");
    let mut text = format!("# Pull an image from this index\ndocker pull {reference}\n");
    if uploads {
        let _ = write!(
            text,
            "\n# Publish an image to this index\ndocker login {host}\ndocker tag {IMAGE_PLACEHOLDER}:{TAG_PLACEHOLDER} {reference}\ndocker push {reference}\n"
        );
    }
    text
}

#[cfg(test)]
mod tests {
    use velodex_http::discovery::BaseUrl;
    use velodex_http::state::IndexDescription;

    use super::index_entry;

    fn description(uploads: bool) -> IndexDescription {
        IndexDescription {
            name: "images".to_owned(),
            route: "root/oci".to_owned(),
            ecosystem: "oci",
            kind: "virtual",
            layers: vec!["root/oci-store".to_owned()],
            uploads,
            volatile_deletes: false,
            upload_to: uploads.then(|| "root/oci-store".to_owned()),
            upstream: None,
            hosted: None,
        }
    }

    #[test]
    fn test_entry_renders_registry_endpoint_and_pull_snippet() {
        let base = BaseUrl::parse("https://registry.example:5000/").unwrap();
        let entry = index_entry(description(false), Some(&base));
        assert_eq!(entry["ecosystem"], "oci");
        assert_eq!(entry["urls"]["registry"], "https://registry.example:5000/v2/");
        assert_eq!(entry["capabilities"]["manifest_push"], false);
        let docker = entry["client_configuration"]["docker"].as_str().unwrap();
        assert!(docker.contains("docker pull registry.example:5000/root/oci/<image>:<tag>"));
        assert!(!docker.contains("docker push"));
    }

    #[test]
    fn test_writable_entry_includes_push_and_login() {
        let base = BaseUrl::parse("https://registry.example/").unwrap();
        let entry = index_entry(description(true), Some(&base));
        assert_eq!(entry["capabilities"]["manifest_push"], true);
        let docker = entry["client_configuration"]["docker"].as_str().unwrap();
        assert!(docker.contains("docker login registry.example"));
        assert!(docker.contains("docker push registry.example/root/oci/<image>:<tag>"));
    }

    #[test]
    fn test_entry_without_base_uses_host_placeholder() {
        let entry = index_entry(description(false), None);
        assert_eq!(entry["urls"]["registry"], "/v2/");
        let docker = entry["client_configuration"]["docker"].as_str().unwrap();
        assert!(docker.contains("docker pull <host>/root/oci/<image>:<tag>"));
    }
}
