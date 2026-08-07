//! Centralized ecosystem plugin registration.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use peryx_core::Ecosystem;
use peryx_driver::discovery::BaseUrl;
use peryx_driver::serving::{CompiledEcosystemSettings, EcosystemCapability, EcosystemPlugin};
use peryx_driver::{AppState, DriverSet};
use utoipa::openapi::PathsBuilder;

pub mod pypi {
    pub use peryx_ecosystem_pypi::*;
}

pub use peryx_ecosystem_pypi::ECOSYSTEM as PYPI;

pub mod oci {
    pub use peryx_ecosystem_oci::*;
}

pub use peryx_ecosystem_oci::ECOSYSTEM as OCI;

#[derive(Debug, Default, Clone, PartialEq, Eq, clap::Args)]
pub struct MirrorOptions {
    #[command(flatten)]
    pub pypi: peryx_ecosystem_pypi::MirrorOptions,
    #[command(flatten)]
    pub oci: peryx_ecosystem_oci::MirrorOptions,
}

impl MirrorOptions {
    #[must_use]
    pub fn overrides(&self, ecosystem: Ecosystem) -> toml::Table {
        if ecosystem == PYPI {
            self.pypi.overrides()
        } else if ecosystem == OCI {
            self.oci.overrides()
        } else {
            toml::Table::new()
        }
    }
}

fn plugins() -> &'static [Arc<dyn EcosystemPlugin>] {
    static PLUGINS: OnceLock<Vec<Arc<dyn EcosystemPlugin>>> = OnceLock::new();
    PLUGINS.get_or_init(|| {
        vec![
            Arc::new(peryx_ecosystem_pypi::PypiPlugin),
            Arc::new(peryx_ecosystem_oci::OciPlugin),
        ]
    })
}

fn plugin(ecosystem: Ecosystem) -> Option<&'static Arc<dyn EcosystemPlugin>> {
    plugins().iter().find(|plugin| plugin.ecosystem() == ecosystem)
}

/// The ecosystem used when an index omits `ecosystem`.
#[must_use]
pub fn default_ecosystem() -> Ecosystem {
    plugins()[0].ecosystem()
}

#[must_use]
pub fn is_installed(ecosystem: Ecosystem) -> bool {
    plugin(ecosystem).is_some()
}

pub fn default_indexes() -> impl Iterator<Item = &'static peryx_ecosystem_contract::DefaultIndex> {
    plugins().iter().flat_map(|plugin| plugin.default_indexes())
}

/// Build the installed driver set for this binary.
pub fn drivers() -> &'static DriverSet {
    static DRIVERS: OnceLock<DriverSet> = OnceLock::new();
    DRIVERS.get_or_init(|| {
        let mut drivers = DriverSet::default();
        register_pypi(&mut drivers);
        register_oci(&mut drivers);
        drivers
    })
}

/// Register all installed ecosystem runtime components into application state.
/// # Errors
///
/// Returns an error when an ecosystem plugin cannot install its runtime services.
pub fn install_drivers<S: std::hash::BuildHasher>(
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
        plugin.install(state, &plugin_settings, distributed)?;
    }
    Ok(())
}

/// Register just the `PyPI` plugin.
pub fn register_pypi(drivers: &mut DriverSet) {
    let current = std::mem::take(drivers);
    *drivers = current.with(peryx_ecosystem_pypi::PypiPlugin.driver());
}

/// Register just the OCI plugin.
pub fn register_oci(drivers: &mut DriverSet) {
    let current = std::mem::take(drivers);
    *drivers = current.with(peryx_ecosystem_oci::OciPlugin.driver());
}

/// Validate one index's ecosystem-owned settings and return any runtime settings needed at install.
///
/// # Errors
/// Returns the implementation's settings error or reports an uninstalled ecosystem.
pub fn compile_index_settings(
    ecosystem: Ecosystem,
    name: &str,
    settings: &toml::Table,
) -> Result<Option<CompiledEcosystemSettings>, String> {
    plugin(ecosystem)
        .ok_or_else(|| format!("ecosystem {ecosystem} is not installed"))?
        .compile_index_settings(name, settings)
}

#[must_use]
pub fn supports(ecosystem: Ecosystem, capability: EcosystemCapability) -> bool {
    plugin(ecosystem).is_some_and(|plugin| plugin.supports(capability))
}

/// Merge ecosystem `OpenAPI` paths into the provided builder.
#[must_use]
pub fn openapi_paths(paths: PathsBuilder) -> PathsBuilder {
    plugins()
        .iter()
        .fold(paths, |paths, plugin| plugin.openapi_paths(paths))
}

/// Resolve the driver that serves one index by ecosystem.
#[must_use]
pub fn driver_for(
    drivers: &DriverSet,
    ecosystem: Ecosystem,
) -> Option<&std::sync::Arc<dyn peryx_driver::serving::EcosystemDriver>> {
    drivers.get(ecosystem)
}

/// Render an ecosystem-owned client configuration snippet.
///
/// # Errors
/// Returns an error when the format is unsupported or the ecosystem is not installed.
pub fn snippet_text(
    ecosystem: Ecosystem,
    base: &BaseUrl,
    route: &str,
    uploads: bool,
    format: &str,
) -> Result<Option<String>, String> {
    plugin(ecosystem)
        .ok_or_else(|| format!("ecosystem {ecosystem} is not installed"))?
        .snippet_text(base, route, uploads, format)
}
