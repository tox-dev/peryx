//! The `OpenAPI` description of peryx's HTTP surface.
//!
//! Built programmatically so it lives next to the handlers and is exercised by tests. Served at
//! `/api-docs/openapi.json` and rendered by the documentation site; regenerate the site copy with
//! `peryx openapi > site/static/openapi.json`.

mod service;
mod shadow;
mod trash;

use peryx_plugin_registry as plugin_registry;
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
    let mut document = serde_json::to_value(openapi()).expect("OpenAPI document always serializes");
    document.sort_all_objects();
    let mut json = serde_json::to_string_pretty(&document).expect("OpenAPI document always serializes");
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
                    "Read-through cache and private artifact service. Ecosystem adapters contribute their \
                     protocol paths. Write operations authenticate with a token accepted by the target \
                     hosted index. Server administration uses a local user's display name and password \
                     with role authorization.",
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
                                "The password is a write-granting access token of the hosted index.",
                            ))
                            .build(),
                    ),
                )
                .security_scheme(
                    "administratorPassword",
                    SecurityScheme::Http(
                        HttpBuilder::new()
                            .scheme(HttpAuthScheme::Basic)
                            .description(Some(
                                "A local server user's display name and password. Each operation checks the user's role against its protected resource.",
                            ))
                            .build(),
                    ),
                )
                .build(),
        ))
        .build()
}

/// Fold every ecosystem's wire-protocol paths into peryx's own service paths.
///
/// Each ecosystem crate describes the protocol it serves; naming them here is the composition root's
/// job, the same place their drivers are installed.
fn paths() -> PathsBuilder {
    let ecosystems = plugin_registry::openapi_paths(PathsBuilder::new());
    shadow::shadow_paths(trash::trash_paths(service::service_paths(ecosystems)))
}
