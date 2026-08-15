use anyhow::bail;
use peryx_storage::blob::BlobStorage;
use peryx_storage::meta::MetaStore;

use crate::config::{BlobStorageConfig, Config};

mod bootstrap;
mod cache;
mod config;
mod fsck;
mod indexes;
mod jobs;
mod policy;
mod purge;
mod quota;
mod retention;
mod revocation;
mod secret;

pub use bootstrap::{bootstrap_administrator, bootstrap_administrator_with_plugins};
pub use cache::{cache, cache_with_plugins};
pub(crate) use config::config_check_with_active_plugins;
pub use config::{config_check, config_check_with_plugins};
pub use indexes::{config_snippet, config_snippet_with_plugins, index, index_with_plugins, init, init_data_dir};
pub(crate) use jobs::job_with_active_plugins;
pub use jobs::{job, job_with_plugins};
pub use policy::{policy, policy_with_plugins};
pub(crate) use purge::referenced_blob_digests_with_drivers;
pub use quota::{quota, quota_with_plugins};
pub use retention::{retention, retention_with_plugins};
pub use revocation::revocation;

/// Reject an offline command that reads or writes the local filesystem blob store when the
/// repository points its blobs at an object store, before the command can mutate metadata or report
/// success against bytes the running server keeps elsewhere.
///
/// # Errors
/// Returns an error when the configured blob backend is not the local filesystem.
pub(crate) fn reject_object_store_blob(config: &Config, command: &str) -> anyhow::Result<()> {
    match config.blob {
        BlobStorageConfig::Filesystem => Ok(()),
        BlobStorageConfig::S3(_) => bail!(
            "{command} is only supported on the filesystem blob backend, but this repository is configured for \
             S3; run it against a filesystem-backed repository"
        ),
    }
}

struct CacheStores {
    meta: MetaStore,
    blobs: BlobStorage,
}

impl CacheStores {
    fn open(config: &Config, plugins: &peryx_plugin_registry::PluginRegistry, writable: bool) -> anyhow::Result<Self> {
        let path = config.data_dir.join("peryx.redb");
        Ok(Self {
            meta: if writable {
                crate::metadata::open_existing(&path, plugins)?
            } else {
                crate::metadata::open_existing_read_only(&path, plugins)?
            },
            blobs: BlobStorage::filesystem(config.data_dir.join("blobs")),
        })
    }
}

fn index_names(config: &Config) -> Vec<&str> {
    let mut names = config
        .indexes
        .iter()
        .map(|index| index.name.as_str())
        .collect::<Vec<_>>();
    names.sort_by_key(|name| std::cmp::Reverse(name.len()));
    names
}

#[cfg(test)]
#[path = "../../tests/unit/tests/app/mod_tests.rs"]
mod tests;
