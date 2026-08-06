//! Centralized ecosystem plugin registration.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use peryx_core::{Ecosystem, EcosystemInstaller};
use peryx_driver::{AppState, DriverSet};
use utoipa::openapi::PathsBuilder;
use toml::Table;

#[cfg(feature = "ecosystem-oci")]
pub use peryx_ecosystem_oci::IndexSettings;

#[cfg(feature = "ecosystem-oci")]
pub type OciIndexSettings = IndexSettings;

#[cfg(not(feature = "ecosystem-oci"))]
pub type OciIndexSettings = ();

#[cfg(feature = "ecosystem-oci")]
pub const TOKEN_SERVICE: &str = peryx_ecosystem_oci::TOKEN_SERVICE;

#[cfg(not(feature = "ecosystem-oci"))]
pub const TOKEN_SERVICE: &str = "peryx";

/// Build the installed driver set for this binary.
pub fn drivers() -> &'static DriverSet {
    static DRIVERS: OnceLock<DriverSet> = OnceLock::new();
    DRIVERS.get_or_init(|| {
        let mut drivers = DriverSet::default();
        #[cfg(feature = "ecosystem-pypi")]
        {
            drivers = drivers.with(Arc::new(peryx_ecosystem_pypi::PypiServing));
        }
        #[cfg(feature = "ecosystem-oci")]
        {
            drivers = drivers.with(Arc::new(peryx_ecosystem_oci::OciRegistry::default()));
        }
        drivers
    })
}

/// Register all installed ecosystem runtime components into application state.
pub fn install_drivers(
    state: &mut AppState,
    oci_settings: &HashMap<String, OciIndexSettings>,
    journal_outbox: bool,
) {
    #[cfg(feature = "ecosystem-pypi")]
    EcosystemInstaller::install(&peryx_ecosystem_pypi::PypiServing, state);

    #[cfg(feature = "ecosystem-oci")]
    EcosystemInstaller::install(
        &peryx_ecosystem_oci::OciInstaller::new(
            oci_settings.iter().map(|(name, settings)| (name.clone(), *settings)),
            journal_outbox,
        ),
        state,
    );
}

/// Compile OCI index settings for the requesting index.
pub fn compile_oci_index_settings(name: &str, settings: &Table) -> Result<OciIndexSettings, String> {
    #[cfg(feature = "ecosystem-oci")]
    {
        return Ok(peryx_ecosystem_oci::IndexSettings::compile(settings)
            .map_err(|reason| format!("compile settings for {name}: {reason}"))?);
    }

    #[cfg(not(feature = "ecosystem-oci"))]
    if !settings.is_empty() {
        return Err(format!(
            "compile settings for {name}: OCI support is not enabled in this peryx build"
        ));
    }
    Ok(())
}

/// Merge ecosystem OpenAPI paths into the provided builder.
pub fn openapi_paths(paths: PathsBuilder) -> PathsBuilder {
    #[cfg(feature = "ecosystem-pypi")]
    let paths = peryx_ecosystem_pypi::openapi::openapi_paths(paths);

    #[cfg(feature = "ecosystem-oci")]
    let paths = peryx_ecosystem_oci::openapi::openapi_paths(paths);

    #[cfg(not(any(feature = "ecosystem-pypi", feature = "ecosystem-oci")))]
    let paths = paths;

    paths
}

/// Resolve the driver that serves one index by ecosystem.
pub fn driver_for<'a>(drivers: &'a DriverSet, ecosystem: Ecosystem) -> Option<&'a std::sync::Arc<dyn peryx_driver::serving::EcosystemDriver>> {
    drivers.get(ecosystem)
}
