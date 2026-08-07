//! Link-time ecosystem plugin registry.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use peryx_core::Ecosystem;
use peryx_driver::discovery::BaseUrl;
use peryx_driver::serving::{CompiledEcosystemSettings, EcosystemCapability, EcosystemPlugin};
use peryx_driver::{AppState, DriverSet};
use utoipa::openapi::PathsBuilder;

pub struct PluginRegistration {
    pub plugin: &'static dyn EcosystemPlugin,
    pub priority: u16,
}

inventory::collect!(PluginRegistration);

fn plugins() -> &'static [&'static dyn EcosystemPlugin] {
    static PLUGINS: OnceLock<Vec<&'static dyn EcosystemPlugin>> = OnceLock::new();
    PLUGINS.get_or_init(|| {
        let mut registrations = inventory::iter::<PluginRegistration>.into_iter().collect::<Vec<_>>();
        registrations.sort_unstable_by_key(|registration| registration.priority);
        let mut ecosystems = HashSet::new();
        let mut priorities = HashSet::new();
        for registration in &registrations {
            assert!(
                ecosystems.insert(registration.plugin.ecosystem()),
                "duplicate ecosystem plugin"
            );
            assert!(priorities.insert(registration.priority), "duplicate plugin priority");
        }
        let plugins = registrations
            .into_iter()
            .map(|registration| registration.plugin)
            .collect::<Vec<_>>();
        assert!(
            !plugins.is_empty(),
            "the binary must link at least one ecosystem plugin"
        );
        plugins
    })
}

fn plugin(ecosystem: Ecosystem) -> Option<&'static dyn EcosystemPlugin> {
    plugins().iter().copied().find(|plugin| plugin.ecosystem() == ecosystem)
}

#[must_use]
pub fn default_ecosystem() -> Ecosystem {
    plugins()[0].ecosystem()
}

#[must_use]
pub fn is_installed(ecosystem: Ecosystem) -> bool {
    plugin(ecosystem).is_some()
}

pub fn default_indexes() -> impl Iterator<Item = &'static peryx_core::DefaultIndex> {
    plugins().iter().flat_map(|plugin| plugin.default_indexes())
}

pub fn drivers() -> &'static DriverSet {
    static DRIVERS: OnceLock<DriverSet> = OnceLock::new();
    DRIVERS.get_or_init(|| {
        plugins()
            .iter()
            .fold(DriverSet::default(), |drivers, plugin| drivers.with(plugin.driver()))
    })
}

/// # Errors
/// Returns an error when an ecosystem plugin cannot install its runtime services.
pub fn install_drivers<S: std::hash::BuildHasher>(
    state: &mut AppState,
    settings: &HashMap<String, CompiledEcosystemSettings, S>,
) -> Result<(), String> {
    install(state, settings, false)
}

/// # Errors
/// Returns an error when a plugin cannot install its distributed services.
pub fn install_distributed_drivers<S: std::hash::BuildHasher>(
    state: &mut AppState,
    settings: &HashMap<String, CompiledEcosystemSettings, S>,
) -> Result<(), String> {
    install(state, settings, true)
}

fn install<S: std::hash::BuildHasher>(
    state: &mut AppState,
    settings: &HashMap<String, CompiledEcosystemSettings, S>,
    distributed: bool,
) -> Result<(), String> {
    for plugin in plugins() {
        let plugin_settings = settings
            .iter()
            .filter(|(_, settings)| settings.ecosystem() == plugin.ecosystem())
            .map(|(name, settings)| (name.as_str(), settings))
            .collect::<Vec<_>>();
        if distributed {
            plugin.install_distributed(state, &plugin_settings)?;
        } else {
            plugin.install(state, &plugin_settings)?;
        }
    }
    Ok(())
}

/// # Errors
/// Returns the implementation's settings error or reports an uninstalled ecosystem.
pub fn compile_index_settings(
    ecosystem: Ecosystem,
    name: &str,
    settings: &toml::Table,
) -> Result<Option<CompiledEcosystemSettings>, String> {
    let Some(plugin) = plugin(ecosystem) else {
        return Err(format!("ecosystem {ecosystem} is not installed"));
    };
    plugin.compile_index_settings(name, settings)
}

#[must_use]
pub fn supports(ecosystem: Ecosystem, capability: EcosystemCapability) -> bool {
    plugin(ecosystem).is_some_and(|plugin| plugin.supports(capability))
}

#[must_use]
pub fn openapi_paths(paths: PathsBuilder) -> PathsBuilder {
    plugins()
        .iter()
        .fold(paths, |paths, plugin| plugin.openapi_paths(paths))
}

/// # Errors
/// Returns an error when the format is unsupported or the ecosystem is not installed.
pub fn snippet_text(
    ecosystem: Ecosystem,
    base: &BaseUrl,
    route: &str,
    uploads: bool,
    format: &str,
) -> Result<Option<String>, String> {
    let Some(plugin) = plugin(ecosystem) else {
        return Err(format!("ecosystem {ecosystem} is not installed"));
    };
    plugin.snippet_text(base, route, uploads, format)
}
