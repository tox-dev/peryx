//! Command actions that do not touch global state.

use anyhow::Context as _;
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;

use crate::config::Config;

mod cache;
mod fsck;
mod indexes;
mod policy;
mod purge;

pub use cache::cache;
pub use indexes::{config_snippet, index, init, init_data_dir};
pub use policy::policy;
pub(crate) use purge::referenced_blob_digests;

struct CacheStores {
    meta: MetaStore,
    blobs: BlobStore,
}

impl CacheStores {
    fn open(config: &Config) -> anyhow::Result<Self> {
        Ok(Self {
            meta: MetaStore::open_existing(config.data_dir.join("peryx.redb"))
                .with_context(|| format!("open metadata store {}", config.data_dir.join("peryx.redb").display()))?,
            blobs: BlobStore::new(config.data_dir.join("blobs")),
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

fn split_page_key(key: &str, index_names: &[&str]) -> (String, String) {
    for name in index_names {
        if let Some(project) = key.strip_prefix(name).and_then(|rest| rest.strip_prefix('/')) {
            return ((*name).to_owned(), project.to_owned());
        }
    }
    key.split_once('/').map_or_else(
        || (key.to_owned(), String::new()),
        |(index, project)| (index.to_owned(), project.to_owned()),
    )
}
