//! The search index composes several ecosystems' indexers: a second `add_indexer` widens the results
//! rather than replacing the first.

mod engine_tests;
mod integration_tests;

use std::sync::Arc;

use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;

use peryx_format::Lexicon;

use crate::state::AppState;

pub(super) static OCI_WORDS: Lexicon = Lexicon {
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

pub(super) fn state(dir: &tempfile::TempDir) -> AppState {
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    AppState::with_clock(meta, blobs, 60, Vec::new(), Arc::new(|| 0))
}
