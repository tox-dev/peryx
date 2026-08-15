use peryx_core::{Ecosystem, Lexicon, LexiconRegistry};
use peryx_index::Index;
use peryx_storage::blob::BlobStorage;
use peryx_storage::meta::MetaStore;

pub struct IndexerCtx<'a> {
    pub indexes: &'a [Index],
    pub meta: &'a MetaStore,
    pub blobs: &'a BlobStorage,
}

impl IndexerCtx<'_> {
    #[must_use]
    pub fn index_at(&self, position: usize) -> &Index {
        &self.indexes[position]
    }
}

pub struct SearchCtx<'a> {
    pub indexer: IndexerCtx<'a>,
    pub lexicons: &'a LexiconRegistry,
}

impl SearchCtx<'_> {
    #[must_use]
    pub fn lexicon(&self, ecosystem: &Ecosystem) -> &'static Lexicon {
        self.lexicons.get(ecosystem)
    }
}
