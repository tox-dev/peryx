mod context_tests;
mod engine_tests;
mod error_tests;
mod frontier_tests;
mod integration_tests;
mod params_tests;
mod verify_tests;

use peryx_core::{Lexicon, LexiconRegistry};
use peryx_index::Index;
use peryx_storage::blob::{BlobStorage, BlobStore};
use peryx_storage::meta::MetaStore;

use crate::{IndexerCtx, SearchCtx};

static ALT_WORDS: Lexicon = Lexicon {
    repository: "service",
    resource: "component",
    resources: "components",
    resource_kind: "component",
    group: "revision",
    groups: "revisions",
    artifact: "asset",
    artifacts: "assets",
    read: "fetch",
    write: "publish",
};

struct Stores {
    meta: MetaStore,
    blobs: BlobStorage,
    indexes: Vec<Index>,
}

impl Stores {
    fn open(dir: &tempfile::TempDir) -> Self {
        Self {
            meta: MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
            blobs: BlobStore::new(dir.path().join("blobs")).into(),
            indexes: Vec::new(),
        }
    }

    fn ctx<'a>(&'a self, lexicons: &'a LexiconRegistry) -> SearchCtx<'a> {
        SearchCtx {
            indexer: self.indexer_ctx(),
            lexicons,
        }
    }

    fn indexer_ctx(&self) -> IndexerCtx<'_> {
        IndexerCtx {
            indexes: &self.indexes,
            meta: &self.meta,
            blobs: &self.blobs,
        }
    }
}
