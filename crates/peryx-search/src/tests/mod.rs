//! The search index composes several ecosystems' indexers: a second `add_indexer` widens the results
//! rather than replacing the first.

mod engine_tests;
mod integration_tests;

use peryx_core::{Lexicon, LexiconRegistry};
use peryx_index::Index;
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;

use crate::context::{IndexerCtx, SearchCtx};

pub static OCI_WORDS: Lexicon = Lexicon {
    server: "registry",
    collection: "repository",
    collections: "repositories",
    search_noun: "image",
    release: "tag",
    releases: "tags",
    artifact: "blob",
    artifacts: "blobs",
    get: "pull",
    put: "push",
};

/// The stores a search context borrows, kept alive for the length of a test.
pub struct Stores {
    meta: MetaStore,
    blobs: BlobStore,
    indexes: Vec<Index>,
}

impl Stores {
    pub(super) fn open(dir: &tempfile::TempDir) -> Self {
        Self {
            meta: MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
            blobs: BlobStore::new(dir.path().join("blobs")),
            indexes: Vec::new(),
        }
    }

    pub(super) fn ctx<'a>(&'a self, lexicons: &'a LexiconRegistry) -> SearchCtx<'a> {
        SearchCtx {
            indexer: IndexerCtx {
                indexes: &self.indexes,
                meta: &self.meta,
                blobs: &self.blobs,
            },
            lexicons,
        }
    }
}
