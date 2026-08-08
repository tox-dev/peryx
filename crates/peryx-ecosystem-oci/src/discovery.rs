//! OCI API discovery and `docker`/`podman` client configuration.
//!
//! The `GET /+api` entry for an OCI index describes its distribution-spec `/v2/` endpoint, the
//! capabilities peryx serves for it, and a copyable `docker pull` (and, when the index accepts
//! writes, `docker push`) setup. The neutral discovery handler wraps this entry alongside every other
//! ecosystem's into one document.

use std::fmt::Write as _;

use peryx_driver::discovery::{BaseUrl, browse_path, link, stats_path};
use peryx_driver::state::IndexDescription;
use serde_json::{Value, json};

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
#[path = "../tests/unit/discovery/tests.rs"]
mod tests;
