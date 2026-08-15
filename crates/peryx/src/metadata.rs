use std::ops::Deref;
use std::path::Path;

use anyhow::{Context as _, ensure};
use peryx_plugin_registry::PluginRegistry;
use peryx_storage::meta::MetaStore;
use tempfile::{NamedTempFile, TempPath};

pub fn open(path: &Path, plugins: &PluginRegistry) -> anyhow::Result<MetaStore> {
    migrate(
        MetaStore::open(path).with_context(|| format!("open metadata store {}", path.display()))?,
        plugins,
    )
}

pub fn open_existing(path: &Path, plugins: &PluginRegistry) -> anyhow::Result<MetaStore> {
    migrate(
        MetaStore::open_existing(path).with_context(|| format!("open metadata store {}", path.display()))?,
        plugins,
    )
}

pub fn open_existing_read_only(path: &Path, plugins: &PluginRegistry) -> anyhow::Result<MetaStore> {
    let store = MetaStore::open_existing_read_only(path)
        .with_context(|| format!("open metadata store {} read-only", path.display()))?;
    if !plugins.has_metadata_migrations() {
        return Ok(store);
    }
    let probe = Probe::copy(path)?;
    let reports = plugins
        .migrate_metadata(&MetaStore::open_existing(&probe.path).context("open metadata schema probe")?)
        .context("check metadata schema")?;
    ensure!(
        reports.iter().all(|report| report.rewritten == 0),
        "metadata store {} requires a schema upgrade; open it with a writable peryx command before retrying",
        path.display()
    );
    Ok(store)
}

pub fn open_existing_copy(path: &Path, plugins: &PluginRegistry) -> anyhow::Result<OpenedMetadata> {
    let source = MetaStore::open_existing_read_only(path)
        .with_context(|| format!("open metadata store {} read-only", path.display()))?;
    if !plugins.has_metadata_migrations() {
        return Ok(OpenedMetadata {
            store: source,
            _probe: None,
        });
    }
    let probe = Probe::copy(path)?;
    drop(source);
    let store = MetaStore::open_existing(&probe.path).context("open copied metadata store")?;
    let store = migrate(store, plugins)?;
    Ok(OpenedMetadata {
        store,
        _probe: Some(probe),
    })
}

fn migrate(store: MetaStore, plugins: &PluginRegistry) -> anyhow::Result<MetaStore> {
    plugins.migrate_metadata(&store).context("migrate metadata")?;
    Ok(store)
}

struct Probe {
    path: TempPath,
}

pub struct OpenedMetadata {
    store: MetaStore,
    _probe: Option<Probe>,
}

impl Deref for OpenedMetadata {
    type Target = MetaStore;

    fn deref(&self) -> &Self::Target {
        &self.store
    }
}

impl Probe {
    fn copy(source: &Path) -> anyhow::Result<Self> {
        let parent = source
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut probe = NamedTempFile::with_prefix_in(".peryx-metadata-probe-", parent)
            .with_context(|| format!("create metadata schema probe beside {}", source.display()))?;
        std::io::copy(
            &mut std::fs::File::open(source).context("open metadata store for schema inspection")?,
            probe.as_file_mut(),
        )
        .context("copy metadata store for schema inspection")?;
        Ok(Self {
            path: probe.into_temp_path(),
        })
    }
}

#[cfg(test)]
#[path = "../tests/unit/tests/metadata_tests.rs"]
mod tests;
