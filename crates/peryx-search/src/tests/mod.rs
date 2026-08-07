//! The search index composes several ecosystems' indexers: a second `add_indexer` widens the results
//! rather than replacing the first.

mod engine_tests;
mod frontier_tests;
mod integration_tests;

use peryx_core::{Lexicon, LexiconRegistry};
use peryx_index::Index;
use peryx_storage::blob::{BlobStorage, BlobStore};
use peryx_storage::meta::MetaStore;

use crate::context::{IndexerCtx, SearchCtx};

pub static ALT_WORDS: Lexicon = Lexicon {
    server: "service",
    collection: "component",
    collections: "components",
    search_noun: "component",
    release: "revision",
    releases: "revisions",
    artifact: "asset",
    artifacts: "assets",
    get: "fetch",
    put: "publish",
};

/// The stores a search context borrows, kept alive for the length of a test.
pub struct Stores {
    meta: MetaStore,
    blobs: BlobStorage,
    indexes: Vec<Index>,
}

impl Stores {
    pub(super) fn open(dir: &tempfile::TempDir) -> Self {
        Self {
            meta: MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
            blobs: BlobStore::new(dir.path().join("blobs")).into(),
            indexes: Vec::new(),
        }
    }

    pub(super) fn ctx<'a>(&'a self, lexicons: &'a LexiconRegistry) -> SearchCtx<'a> {
        SearchCtx {
            indexer: self.indexer_ctx(),
            lexicons,
        }
    }

    pub(super) fn indexer_ctx(&self) -> IndexerCtx<'_> {
        IndexerCtx {
            indexes: &self.indexes,
            meta: &self.meta,
            blobs: &self.blobs,
        }
    }
}
