//! Built programmatically so it lives next to the handlers and is exercised by tests. Served at
//! `/api-docs/openapi.json` and rendered from the documentation site's staged copy.

mod service;
mod trash;

use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::openapi::{
    ComponentsBuilder, ContactBuilder, InfoBuilder, LicenseBuilder, OpenApi, OpenApiBuilder, PathsBuilder,
    ServerBuilder,
};

/// # Panics
/// Panics if serialization of the fixed schema fails.
#[must_use]
pub fn openapi_json() -> String {
    openapi_json_with_plugins(&crate::compiled_plugins())
}

#[must_use]
pub fn openapi_json_with_plugins(plugins: &peryx_plugin_registry::PluginRegistry) -> String {
    openapi_json_for_with_plugins(peryx_ha::AvailabilityResources::Distributed, plugins)
}

#[must_use]
pub fn openapi_json_for(resources: peryx_ha::AvailabilityResources) -> String {
    openapi_json_for_with_plugins(resources, &crate::compiled_plugins())
}

/// # Panics
/// Panics if the generated document cannot be represented or formatted as JSON.
#[must_use]
pub fn openapi_json_for_with_plugins(
    resources: peryx_ha::AvailabilityResources,
    plugins: &peryx_plugin_registry::PluginRegistry,
) -> String {
    let mut document =
        serde_json::to_value(openapi_for_with_plugins(resources, plugins)).expect("OpenAPI document always serializes");
    document.sort_all_objects();
    let mut json = serde_json::to_string_pretty(&document).expect("OpenAPI document always serializes");
    json.push('\n');
    json
}

#[must_use]
pub fn openapi() -> OpenApi {
    openapi_with_plugins(&crate::compiled_plugins())
}

#[must_use]
pub fn openapi_with_plugins(plugins: &peryx_plugin_registry::PluginRegistry) -> OpenApi {
    openapi_for_with_plugins(peryx_ha::AvailabilityResources::Distributed, plugins)
}

#[must_use]
pub fn openapi_for(resources: peryx_ha::AvailabilityResources) -> OpenApi {
    openapi_for_with_plugins(resources, &crate::compiled_plugins())
}

#[must_use]
pub fn openapi_for_with_plugins(
    resources: peryx_ha::AvailabilityResources,
    plugins: &peryx_plugin_registry::PluginRegistry,
) -> OpenApi {
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
        .paths(paths(resources.has_routes(), plugins))
        .components(Some(
            ComponentsBuilder::new()
                .security_scheme(
                    "writeToken",
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
                    "uploadToken",
                    SecurityScheme::Http(
                        HttpBuilder::new()
                            .scheme(HttpAuthScheme::Basic)
                            .description(Some("Deprecated alias for `writeToken`."))
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

fn paths(distributed: bool, plugins: &peryx_plugin_registry::PluginRegistry) -> PathsBuilder {
    let ecosystems = plugins.openapi_paths(PathsBuilder::new());
    let services = service::service_paths(ecosystems);
    let services = if distributed {
        peryx_ha_distributed::availability_paths(services)
    } else {
        services
    };
    trash::trash_paths(services)
}
