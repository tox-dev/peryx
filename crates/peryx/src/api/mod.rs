//! The `OpenAPI` description of peryx's HTTP surface.
//!
//! Built programmatically so it lives next to the handlers and is exercised by tests. Served at
//! `/api-docs/openapi.json` and rendered by the documentation site; regenerate the site copy with
//! `peryx openapi > site/static/openapi.json`.

mod oci;
mod pypi;
mod service;
mod shared;

use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::openapi::{
    ComponentsBuilder, ContactBuilder, InfoBuilder, LicenseBuilder, OpenApi, OpenApiBuilder, PathsBuilder,
    ServerBuilder,
};

/// The document as pretty JSON, shared by the HTTP endpoint and the `peryx openapi` subcommand.
///
/// # Panics
/// Never in practice: the document is a static structure that always serializes.
#[must_use]
pub fn openapi_json() -> String {
    let mut json = serde_json::to_string_pretty(&openapi()).expect("OpenAPI document always serializes");
    json.push('\n');
    json
}

/// Build the `OpenAPI` 3.1 document for the peryx HTTP API.
#[must_use]
pub fn openapi() -> OpenApi {
    OpenApiBuilder::new()
        .info(
            InfoBuilder::new()
                .title("peryx")
                .version(env!("CARGO_PKG_VERSION"))
                .description(Some(
                    "Read-through cache and private index for multiple ecosystems. A PyPI index serves \
                     the Simple API under `/{route}/`, where `{route}` is the index's route (for example \
                     `root/pypi`); an OCI index serves the distribution-spec registry under `/v2/`. Write \
                     operations authenticate with HTTP Basic where the password is the target hosted \
                     index's upload token and the username is ignored.",
                ))
                .contact(Some(
                    ContactBuilder::new()
                        .name(Some("tox-dev"))
                        .url(Some("https://github.com/tox-dev/peryx"))
                        .build(),
                ))
                .license(Some(LicenseBuilder::new().name("MIT").build()))
                .build(),
        )
        .servers(Some([ServerBuilder::new()
            .url("http://127.0.0.1:4433")
            .description(Some("A local peryx with the default configuration"))
            .build()]))
        .paths(paths())
        .components(Some(
            ComponentsBuilder::new()
                .security_scheme(
                    "uploadToken",
                    SecurityScheme::Http(
                        HttpBuilder::new()
                            .scheme(HttpAuthScheme::Basic)
                            .description(Some(
                                "Any username; the password is the hosted index's `upload_token` \
                                 (the pypi.org `__token__` convention)",
                            ))
                            .build(),
                    ),
                )
                .build(),
        ))
        .build()
}

fn paths() -> PathsBuilder {
    service::service_paths(oci::oci_paths(pypi::route_paths(PathsBuilder::new())))
}
