use std::num::NonZeroUsize;
use std::ops::ControlFlow;
use std::sync::Arc;

use peryx_core::LexiconRegistry;

use super::Stores;
use crate::context::IndexerCtx;
use crate::engine::RebuildProgress;
use crate::{PackageDocument, PackageIndexer, PackageSearch, PackageSource, SEARCH_VIEW, SearchError, SearchParams};

struct OneDoc;

impl PackageIndexer for OneDoc {
    fn documents(&self, _ctx: &IndexerCtx<'_>) -> Result<Vec<PackageDocument>, SearchError> {
        Ok(vec![PackageDocument {
            display_name: "pkg".to_owned(),
            normalized_name: "pkg".to_owned(),
            route: "root".to_owned(),
            index: "root".to_owned(),
            ecosystem: "alpha".to_owned(),
            source: PackageSource::Cached,
            available_locally: false,
            summary: None,
            text: "pkg".to_owned(),
        }])
    }
}

/// Raise the store's journal serial by `count` so a refresh has a higher frontier to persist.
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
    let mut search = PackageSearch::in_memory();
    search.add_indexer(Arc::new(OneDoc));
    advance_serial(&stores, 2);

    assert_eq!(stores.meta.view_frontier(SEARCH_VIEW).unwrap(), None);
    search.search(&stores.ctx(&lexicons), SearchParams::default()).unwrap();

    assert_eq!(stores.meta.view_frontier(SEARCH_VIEW).unwrap(), Some(2));
}

#[test]
fn test_rebuild_persists_the_store_frontier_it_indexed() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let mut search = PackageSearch::in_memory();
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
    let mut search = PackageSearch::in_memory();
    search.add_indexer(Arc::new(OneDoc));
    advance_serial(&stores, 3);
    search.search(&stores.ctx(&lexicons), SearchParams::default()).unwrap();
    assert_eq!(stores.meta.view_frontier(SEARCH_VIEW).unwrap(), Some(3));

    // A per-project invalidation refreshes only its document, yet the lazy refresh still records the
    // frontier it indexed so a replica's readable frontier advances over the mutation.
    advance_serial(&stores, 2);
    search.invalidate_project("pkg");
    search.search(&stores.ctx(&lexicons), SearchParams::default()).unwrap();

    assert_eq!(stores.meta.view_frontier(SEARCH_VIEW).unwrap(), Some(5));
}

#[test]
fn test_update_project_leaves_the_view_frontier_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let mut search = PackageSearch::in_memory();
    search.add_indexer(Arc::new(OneDoc));
    advance_serial(&stores, 3);

    // The scoped update commits a document but never records a frontier; the apply path advances it only
    // after every affected project is current, so this alone must not move it.
    search
        .update_project(
            &[PackageDocument {
                display_name: "pkg".to_owned(),
                normalized_name: "pkg".to_owned(),
                route: "root".to_owned(),
                index: "root".to_owned(),
                ecosystem: "alpha".to_owned(),
                source: PackageSource::Cached,
                available_locally: false,
                summary: None,
                text: "pkg".to_owned(),
            }],
            &crate::project_key("root", "pkg"),
        )
        .unwrap();

    assert_eq!(stores.meta.view_frontier(SEARCH_VIEW).unwrap(), None);
}
