use std::io::Write;
use std::path::Path;

use anyhow::{Context as _, bail};
use peryx_core::Ecosystem;
use peryx_storage::blob::BlobStorage;

use crate::config::Config;

/// # Errors
/// Returns an error if the repository uses an object-store blob backend, the data directory cannot
/// be opened, the selected index cannot accept imported files, its ecosystem does not support
/// directory import, or output fails.
pub fn import_dir(config: &Config, selector: &str, dir: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    import_dir_with_plugins(config, &crate::compiled_plugins(), selector, dir, out)
}

/// # Errors
/// Returns an error if the repository uses object storage, the directory or data store cannot be opened,
/// the index cannot accept imports, or its ecosystem import fails.
pub fn import_dir_with_plugins(
    config: &Config,
    plugins: &peryx_plugin_registry::PluginRegistry,
    selector: &str,
    dir: &Path,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    crate::app::reject_object_store_blob(config, "import")?;
    if !dir.is_dir() {
        bail!("import directory {} does not exist", dir.display());
    }
    let plugins = crate::server::activate_plugins(config, plugins)?;
    let target = import_target(config, &plugins, selector)?;
    std::fs::create_dir_all(&config.data_dir)
        .context(format!("create data directory {}", config.data_dir.display()))?;
    let meta = crate::metadata::open(&config.data_dir.join("peryx.redb"), &plugins)?;
    let blobs = BlobStorage::filesystem(config.data_dir.join("blobs"));
    let driver = plugins
        .drivers()
        .get_import(&target.ecosystem)
        .context(format!("no import driver for the {} ecosystem", target.ecosystem))?;
    driver
        .import_dir(&meta, &blobs, &target.name, &target.route, dir, out)
        .map_err(anyhow::Error::msg)
}

#[derive(Debug)]
struct ImportTarget {
    name: String,
    route: String,
    ecosystem: Ecosystem,
}

fn import_target(
    config: &Config,
    plugins: &peryx_plugin_registry::PluginRegistry,
    selector: &str,
) -> anyhow::Result<ImportTarget> {
    let indexes = crate::server::build_indexes_with_plugins(&config.indexes, &config.auth, config.offline, plugins)?;
    let position = indexes
        .iter()
        .position(|index| index.name == selector)
        .or_else(|| indexes.iter().position(|index| index.route == selector))
        .context(format!("unknown index {selector:?}"))?;
    let index = &indexes[position];
    match &index.kind {
        peryx_driver::IndexKind::Hosted { .. } => Ok(ImportTarget {
            name: index.name.clone(),
            route: index.route.clone(),
            ecosystem: index.ecosystem.clone(),
        }),
        peryx_driver::IndexKind::Virtual {
            write_target: Some(write_target),
            ..
        } => Ok(ImportTarget {
            name: indexes[*write_target].name.clone(),
            route: index.route.clone(),
            ecosystem: index.ecosystem.clone(),
        }),
        peryx_driver::IndexKind::Virtual { write_target: None, .. } => {
            bail!("index {selector:?} has no write target")
        }
        peryx_driver::IndexKind::Cached { .. } => bail!("index {selector:?} is read-only"),
    }
}
