//! Built programmatically so it lives next to the handlers and is exercised by tests. Served at
//! `/api-docs/openapi.json` and rendered from the documentation site's staged copy.

mod service;
mod trash;

use peryx_driver::route_auth::{ApiScheme, ReadExposure};
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
    openapi_json_for_with_plugins(peryx_ha::AvailabilityResources::Distributed, plugins, STANDALONE_READS)
}

/// The exposure a document generated outside a running server describes. A deployment serves its own
/// document from its own configuration; this one names every credential a read can carry, because the
/// reader has not told us whether their indexes restrict reads.
const STANDALONE_READS: ReadExposure = ReadExposure::Protected;

#[must_use]
pub fn openapi_json_for(resources: peryx_ha::AvailabilityResources) -> String {
    openapi_json_for_with_plugins(resources, &crate::compiled_plugins(), STANDALONE_READS)
}

/// # Panics
/// Panics if the generated document cannot be represented or formatted as JSON.
#[must_use]
pub fn openapi_json_for_with_plugins(
    resources: peryx_ha::AvailabilityResources,
    plugins: &peryx_plugin_registry::PluginRegistry,
    reads: ReadExposure,
) -> String {
    let mut document = serde_json::to_value(openapi_for_with_plugins(resources, plugins, reads))
        .expect("OpenAPI document always serializes");
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
    openapi_for_with_plugins(peryx_ha::AvailabilityResources::Distributed, plugins, STANDALONE_READS)
}

#[must_use]
pub fn openapi_for(resources: peryx_ha::AvailabilityResources) -> OpenApi {
    openapi_for_with_plugins(resources, &crate::compiled_plugins(), STANDALONE_READS)
}

#[must_use]
pub fn openapi_for_with_plugins(
    resources: peryx_ha::AvailabilityResources,
    plugins: &peryx_plugin_registry::PluginRegistry,
    reads: ReadExposure,
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
        .paths(paths(resources.has_routes(), plugins, reads))
        .components(Some(
            ApiScheme::ALL
                .into_iter()
                .fold(ComponentsBuilder::new(), |components, scheme| {
                    components.security_scheme(scheme.name(), scheme.declaration())
                })
                .build(),
        ))
        .build()
}

fn paths(distributed: bool, plugins: &peryx_plugin_registry::PluginRegistry, reads: ReadExposure) -> PathsBuilder {
    let ecosystems = plugins.openapi_paths(PathsBuilder::new(), reads);
    let services = service::service_paths(ecosystems);
    let services = if distributed {
        peryx_ha_distributed::availability_paths(services)
    } else {
        services
    };
    trash::trash_paths(services)
}
