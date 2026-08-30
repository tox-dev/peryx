use std::fs::File;
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
    let user_names_require_migration = store.user_names_require_migration().context("check user-name schema")?;
    if !user_names_require_migration && !plugins.has_metadata_migrations() {
        return Ok(store);
    }
    let plugins_require_migration = plugins
        .dry_run_metadata_migrations(&store)
        .context("check metadata schema")?
        .iter()
        .any(|report| report.rewritten != 0);
    ensure!(
        !user_names_require_migration && !plugins_require_migration,
        "metadata store {} requires a schema upgrade; open it with a writable peryx command before retrying",
        path.display()
    );
    Ok(store)
}

pub fn open_existing_copy(source: File, path: &Path, plugins: &PluginRegistry) -> anyhow::Result<OpenedMetadata> {
    let probe = Probe::copy(source, path)?;
    let source = MetaStore::open_existing_read_only(&probe.path)
        .with_context(|| format!("open metadata store {} read-only", path.display()))?;
    if !source
        .user_names_require_migration()
        .context("check user-name schema")?
        && !plugins.has_metadata_migrations()
    {
        return Ok(OpenedMetadata {
            store: source,
            _probe: probe,
        });
    }
    drop(source);
    let store = MetaStore::open_existing(&probe.path).context("open copied metadata store")?;
    let store = migrate(store, plugins)?;
    Ok(OpenedMetadata { store, _probe: probe })
}

fn migrate(store: MetaStore, plugins: &PluginRegistry) -> anyhow::Result<MetaStore> {
    store.migrate_user_names().context("migrate user names")?;
    plugins.migrate_metadata(&store).context("migrate metadata")?;
    Ok(store)
}

struct Probe {
    path: TempPath,
}

pub struct OpenedMetadata {
    store: MetaStore,
    _probe: Probe,
}

impl Deref for OpenedMetadata {
    type Target = MetaStore;

    fn deref(&self) -> &Self::Target {
        &self.store
    }
}

impl Probe {
    fn copy(mut source: File, path: &Path) -> anyhow::Result<Self> {
        let mut probe = NamedTempFile::new().context(format!("create metadata schema probe for {}", path.display()))?;
        std::io::copy(&mut source, probe.as_file_mut()).context("copy metadata store for schema inspection")?;
        Ok(Self {
            path: probe.into_temp_path(),
        })
    }
}

#[cfg(test)]
#[path = "../tests/unit/tests/metadata_tests.rs"]
mod tests;
