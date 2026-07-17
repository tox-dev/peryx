//! Importing local artifact files into a hosted index: resolve the upload target from the index
//! topology, then hand the directory to the target ecosystem's driver.

use std::io::Write;
use std::path::Path;

use anyhow::{Context as _, bail};
use peryx_core::Ecosystem;
use peryx_storage::blob::BlobStorage;
use peryx_storage::meta::MetaStore;

use crate::config::Config;

/// Import local artifact files into a hosted index.
///
/// # Errors
/// Returns an error if the data directory cannot be opened, the selected index cannot accept
/// imported files, its ecosystem does not support directory import, or output fails.
pub fn import_dir(config: &Config, selector: &str, dir: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    if !dir.is_dir() {
        bail!("import directory {} does not exist", dir.display());
    }
    let target = import_target(config, selector)?;
    std::fs::create_dir_all(&config.data_dir)
        .context(format!("create data directory {}", config.data_dir.display()))?;
    let open_context = format!("open metadata store {}", config.data_dir.join("peryx.redb").display());
    let meta = MetaStore::open(config.data_dir.join("peryx.redb")).context(open_context)?;
    let blobs = BlobStorage::filesystem(config.data_dir.join("blobs"));
    let driver = crate::server::drivers()
        .get(target.ecosystem)
        .context(format!("no driver for the {} ecosystem", target.ecosystem))?;
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

fn import_target(config: &Config, selector: &str) -> anyhow::Result<ImportTarget> {
    let indexes = crate::server::build_indexes(&config.indexes, &config.auth, config.offline)?;
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
            ecosystem: index.ecosystem,
        }),
        peryx_driver::IndexKind::Virtual {
            upload: Some(upload), ..
        } => Ok(ImportTarget {
            name: indexes[*upload].name.clone(),
            route: index.route.clone(),
            ecosystem: index.ecosystem,
        }),
        peryx_driver::IndexKind::Virtual { upload: None, .. } => {
            bail!("index {selector:?} has no hosted upload target")
        }
        peryx_driver::IndexKind::Cached { .. } => bail!("index {selector:?} is read-only"),
    }
}
