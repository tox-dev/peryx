use peryx_plugin_registry::PluginRegistry;

pub(crate) fn compiled_plugins() -> PluginRegistry {
    PluginRegistry::new(vec![
        #[cfg(feature = "composition-pypi")]
        peryx_ecosystem_pypi::registration(),
        #[cfg(feature = "composition-oci")]
        peryx_ecosystem_oci::registration(),
    ])
    .expect("the composition root has unique plugin registrations")
}

pub mod api;
pub mod app;
pub mod cli;
pub mod config;
pub mod logging;
mod metadata;
pub mod operator;
pub mod prefetch;
pub mod process;
pub mod replication;
pub mod server;

#[cfg(test)]
#[path = "../tests/unit/tests/mod.rs"]
mod tests;
