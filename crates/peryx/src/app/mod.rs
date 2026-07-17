//! Command actions that do not touch global state.

use anyhow::Context as _;
use peryx_storage::blob::BlobStorage;
use peryx_storage::meta::MetaStore;

use crate::config::Config;

mod cache;
mod fsck;
mod indexes;
mod jobs;
mod policy;
mod purge;

pub use cache::cache;
pub use indexes::{config_snippet, index, init, init_data_dir};
pub use jobs::job;
pub use policy::policy;
pub(crate) use purge::referenced_blob_digests;

struct CacheStores {
    meta: MetaStore,
    blobs: BlobStorage,
}

impl CacheStores {
    fn open(config: &Config) -> anyhow::Result<Self> {
        Ok(Self {
            meta: MetaStore::open_existing(config.data_dir.join("peryx.redb"))
                .with_context(|| format!("open metadata store {}", config.data_dir.join("peryx.redb").display()))?,
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
