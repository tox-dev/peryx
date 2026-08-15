use std::num::NonZeroUsize;
use std::ops::ControlFlow;
use std::sync::Arc;

use peryx_core::LexiconRegistry;

use super::Stores;
use crate::{
    ContentSource, IndexerCtx, RebuildProgress, SEARCH_VIEW, SearchDocument, SearchDocumentProvider, SearchError,
    SearchIndex, SearchParams,
};

struct OneDoc;

impl SearchDocumentProvider for OneDoc {
    fn documents(&self, _ctx: &IndexerCtx<'_>) -> Result<Vec<SearchDocument>, SearchError> {
        Ok(vec![SearchDocument {
            display_label: "pkg".to_owned(),
            resource_key: "pkg".to_owned(),
            route: "root".to_owned(),
            index: "root".to_owned(),
            ecosystem: "alpha".to_owned(),
            source: ContentSource::Cached,
            available_locally: false,
            summary: None,
            text: "pkg".to_owned(),
        }])
    }
}

fn advance_serial(stores: &Stores, count: usize) {
    stores
        .meta
        .commit_driver_txn::<(), peryx_storage::meta::MetaError>(|txn| {
            for entry in 0..count {
                txn.put(&format!("k{entry}"), b"v")?;
            }
            Ok(((), (0..count).map(|entry| format!("j{entry}").into_bytes()).collect()))
        })
        .unwrap();
}

#[test]
fn test_lazy_refresh_persists_the_store_frontier_it_indexed() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let mut search = SearchIndex::in_memory();
    search.add_indexer(Arc::new(OneDoc));
    advance_serial(&stores, 2);

    search.search(&stores.ctx(&lexicons), SearchParams::default()).unwrap();

    assert_eq!(stores.meta.view_frontier(SEARCH_VIEW).unwrap(), Some(2));
}

#[test]
fn test_rebuild_persists_the_store_frontier_it_indexed() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let mut search = SearchIndex::in_memory();
    search.add_indexer(Arc::new(OneDoc));
    advance_serial(&stores, 3);

    search
        .rebuild(
            &stores.indexer_ctx(),
            NonZeroUsize::new(4).unwrap(),
            &mut |_: RebuildProgress| ControlFlow::Continue(()),
        )
        .unwrap();

    assert_eq!(stores.meta.view_frontier(SEARCH_VIEW).unwrap(), Some(3));
}

#[test]
fn test_scoped_refresh_persists_the_store_frontier_it_indexed() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let mut search = SearchIndex::in_memory();
    search.add_indexer(Arc::new(OneDoc));
    advance_serial(&stores, 3);
    search.search(&stores.ctx(&lexicons), SearchParams::default()).unwrap();
    assert_eq!(stores.meta.view_frontier(SEARCH_VIEW).unwrap(), Some(3));

    advance_serial(&stores, 2);
    search.invalidate_resource("pkg");
    search.search(&stores.ctx(&lexicons), SearchParams::default()).unwrap();

    assert_eq!(stores.meta.view_frontier(SEARCH_VIEW).unwrap(), Some(5));
}
