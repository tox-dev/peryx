use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex, OnceLock, Weak};

use peryx_core::LexiconRegistry;

use super::Stores;
use crate::{
    ContentSource, IndexerCtx, RebuildOutcome, RebuildProgress, ResourceUpdate, SEARCH_VIEW, SearchDocument,
    SearchDocumentProvider, SearchError, SearchIndex, SearchParams,
};

struct NamedDocs(Arc<Mutex<Vec<String>>>);

impl SearchDocumentProvider for NamedDocs {
    fn documents(&self, _ctx: &IndexerCtx<'_>) -> Result<Vec<SearchDocument>, SearchError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|name| SearchDocument {
                display_label: name.clone(),
                resource_key: name.clone(),
                route: "root".to_owned(),
                index: "root".to_owned(),
                ecosystem: "alpha".to_owned(),
                source: ContentSource::Cached,
                available_locally: false,
                summary: None,
                text: name.clone(),
            })
            .collect())
    }
}

struct InvalidEcosystemDocs;

impl SearchDocumentProvider for InvalidEcosystemDocs {
    fn documents(&self, _ctx: &IndexerCtx<'_>) -> Result<Vec<SearchDocument>, SearchError> {
        let mut document = artifact_doc("pkg", "pkg");
        document.ecosystem = "Invalid".to_owned();
        Ok(vec![document])
    }
}

struct CountingDocs {
    calls: Arc<AtomicUsize>,
    advance_serial: bool,
    names: Vec<&'static str>,
}

impl SearchDocumentProvider for CountingDocs {
    fn documents(&self, ctx: &IndexerCtx<'_>) -> Result<Vec<SearchDocument>, SearchError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if self.advance_serial {
            ctx.meta.next_serial().unwrap();
        }
        Ok(self.names.iter().map(|name| artifact_doc(name, name)).collect())
    }
}

fn total(search: &SearchIndex, stores: &Stores, lexicons: &LexiconRegistry) -> usize {
    search
        .search(&stores.ctx(lexicons), SearchParams::default())
        .unwrap()
        .total
}

fn no_cancel(_: RebuildProgress) -> ControlFlow<()> {
    ControlFlow::Continue(())
}

#[test]
fn test_open_rebuilds_when_the_on_disk_schema_changed() {
    let dir = tempfile::tempdir().unwrap();
    let mut legacy = tantivy::schema::Schema::builder();
    legacy.add_text_field("legacy", tantivy::schema::TEXT);
    tantivy::Index::builder()
        .schema(legacy.build())
        .create_in_dir(dir.path())
        .expect("create the legacy index");
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let search = crate::SearchIndex::open(dir.path()).expect("open rebuilds a mismatched index");

    assert_eq!(total(&search, &stores, &lexicons), 0);
}

#[test]
fn test_open_discards_an_interrupted_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("search");
    let marker = path.with_extension("rebuilding");
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let mut search = SearchIndex::open(&path).unwrap();
    search.add_indexer(Arc::new(NamedDocs(Arc::new(Mutex::new(vec!["stale".to_owned()])))));
    search
        .rebuild(&stores.indexer_ctx(), NonZeroUsize::new(1).unwrap(), &mut no_cancel)
        .unwrap();
    assert_eq!(total(&search, &stores, &lexicons), 1);
    drop(search);
    std::fs::write(&marker, []).unwrap();

    let search = SearchIndex::open(&path).unwrap();

    assert_eq!((total(&search, &stores, &lexicons), marker.exists()), (0, false));
}

#[test]
fn test_search_rejects_an_invalid_indexed_ecosystem() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let mut search = SearchIndex::in_memory();
    search.add_indexer(Arc::new(InvalidEcosystemDocs));

    assert!(matches!(
        search.search(&stores.ctx(&lexicons), SearchParams::default()),
        Err(SearchError::InvalidEcosystem(value)) if value == "Invalid"
    ));
}

#[test]
fn test_rebuild_publishes_new_documents_without_an_epoch_bump() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let names = Arc::new(Mutex::new(vec!["x".to_owned()]));
    let mut search = SearchIndex::in_memory();
    search.add_indexer(Arc::new(NamedDocs(names.clone())));
    assert_eq!(total(&search, &stores, &lexicons), 1);

    *names.lock().unwrap() = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
    let outcome = search
        .rebuild(&stores.indexer_ctx(), NonZeroUsize::new(2).unwrap(), &mut no_cancel)
        .unwrap();

    assert_eq!(outcome, RebuildOutcome::Published { documents: 3 });
    assert_eq!(total(&search, &stores, &lexicons), 3);
}

#[test]
fn test_rebuild_to_an_empty_index_commits_once() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let names = Arc::new(Mutex::new(vec!["a".to_owned()]));
    let mut search = SearchIndex::in_memory();
    search.add_indexer(Arc::new(NamedDocs(names.clone())));
    assert_eq!(total(&search, &stores, &lexicons), 1);

    names.lock().unwrap().clear();
    let outcome = search
        .rebuild(&stores.indexer_ctx(), NonZeroUsize::new(4).unwrap(), &mut no_cancel)
        .unwrap();

    assert_eq!(outcome, RebuildOutcome::Published { documents: 0 });
    assert_eq!(total(&search, &stores, &lexicons), 0);
}

#[test]
fn test_rebuild_cancelled_before_the_first_chunk_keeps_the_served_index() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let names = Arc::new(Mutex::new(vec!["x".to_owned(), "y".to_owned()]));
    let mut search = SearchIndex::in_memory();
    search.add_indexer(Arc::new(NamedDocs(names.clone())));
    assert_eq!(total(&search, &stores, &lexicons), 2);

    *names.lock().unwrap() = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
    let outcome = search
        .rebuild(&stores.indexer_ctx(), NonZeroUsize::new(1).unwrap(), &mut |_| {
            ControlFlow::Break(())
        })
        .unwrap();

    assert_eq!(outcome, RebuildOutcome::Aborted { documents: 0 });
    assert_eq!(total(&search, &stores, &lexicons), 2);
}

#[test]
fn test_rebuild_cancelled_after_a_chunk_does_not_expose_partial_results() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let names = Arc::new(Mutex::new(vec!["x".to_owned(), "y".to_owned()]));
    let mut search = SearchIndex::in_memory();
    search.add_indexer(Arc::new(NamedDocs(names.clone())));
    assert_eq!(total(&search, &stores, &lexicons), 2);

    *names.lock().unwrap() = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
    let mut chunks = 0;
    let outcome = search
        .rebuild(&stores.indexer_ctx(), NonZeroUsize::new(1).unwrap(), &mut |_| {
            chunks += 1;
            if chunks > 1 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })
        .unwrap();

    assert_eq!(outcome, RebuildOutcome::Aborted { documents: 1 });
    assert_eq!(total(&search, &stores, &lexicons), 2);
}

#[test]
fn test_cancelled_rebuild_does_not_leak_into_a_later_scoped_update() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let names = Arc::new(Mutex::new(vec!["x".to_owned(), "y".to_owned()]));
    let mut search = SearchIndex::in_memory();
    search.add_indexer(Arc::new(NamedDocs(names.clone())));
    assert_eq!(total(&search, &stores, &lexicons), 2);

    *names.lock().unwrap() = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
    let mut chunks = 0;
    let outcome = search
        .rebuild(&stores.indexer_ctx(), NonZeroUsize::new(1).unwrap(), &mut |_| {
            chunks += 1;
            if chunks > 1 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })
        .unwrap();
    assert_eq!(outcome, RebuildOutcome::Aborted { documents: 1 });

    search
        .update_resource(&[artifact_doc("x", "x")], &crate::document_key("root", "x"))
        .unwrap();

    assert_eq!(hits(&search, &stores, &lexicons, "x"), 1, "the scoped update keeps x");
    assert_eq!(hits(&search, &stores, &lexicons, "y"), 1, "the prior y is still served");
    assert_eq!(
        hits(&search, &stores, &lexicons, "a"),
        0,
        "the cancelled chunk stays hidden"
    );
    assert_eq!(
        total(&search, &stores, &lexicons),
        2,
        "only the prior two resources remain"
    );
}

#[test]
fn test_on_disk_rebuild_marks_then_clears_the_in_flight_marker() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("search");
    let marker = Path::new(&path).with_extension("rebuilding");
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let names = Arc::new(Mutex::new(vec!["a".to_owned(), "b".to_owned()]));
    let mut search = SearchIndex::open(&path).unwrap();
    search.add_indexer(Arc::new(NamedDocs(names)));

    let mut seen_marker = false;
    let outcome = search
        .rebuild(&stores.indexer_ctx(), NonZeroUsize::new(1).unwrap(), &mut |_| {
            seen_marker |= marker.exists();
            ControlFlow::Continue(())
        })
        .unwrap();

    assert_eq!(outcome, RebuildOutcome::Published { documents: 2 });
    assert!(seen_marker, "the marker records an in-flight on-disk rebuild");
    assert!(!marker.exists(), "a published rebuild clears its marker");
    assert_eq!(total(&search, &stores, &lexicons), 2);
}

#[test]
fn test_cancelled_on_disk_rebuild_clears_the_marker() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("search");
    let marker = Path::new(&path).with_extension("rebuilding");
    let stores = Stores::open(&dir);
    let names = Arc::new(Mutex::new(vec!["a".to_owned()]));
    let mut search = SearchIndex::open(&path).unwrap();
    search.add_indexer(Arc::new(NamedDocs(names)));

    let outcome = search
        .rebuild(&stores.indexer_ctx(), NonZeroUsize::new(1).unwrap(), &mut |_| {
            ControlFlow::Break(())
        })
        .unwrap();
    assert_eq!(outcome, RebuildOutcome::Aborted { documents: 0 });
    assert!(!marker.exists(), "rollback clears the in-flight marker");

    drop(search);
    SearchIndex::open(&path).unwrap();
    assert!(!marker.exists(), "reopening keeps the rolled-back index");
}

#[test]
fn test_rebuild_reports_marker_removal_failure() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("search");
    let marker = Path::new(&path).with_extension("rebuilding");
    let stores = Stores::open(&dir);
    let names = Arc::new(Mutex::new(vec!["a".to_owned()]));
    let mut search = SearchIndex::open(&path).unwrap();
    search.add_indexer(Arc::new(NamedDocs(names)));
    let mut replaced = false;

    let error = search
        .rebuild(&stores.indexer_ctx(), NonZeroUsize::new(1).unwrap(), &mut |_| {
            if !replaced {
                std::fs::remove_file(&marker).unwrap();
                std::fs::create_dir(&marker).unwrap();
                replaced = true;
            }
            ControlFlow::Continue(())
        })
        .unwrap_err();

    assert!(matches!(error, SearchError::Io(_)));
    assert!(marker.is_dir());
}

#[test]
fn test_search_during_a_rebuild_serves_the_prior_index() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let names = Arc::new(Mutex::new(vec!["x".to_owned(), "y".to_owned()]));
    let mut search = SearchIndex::in_memory();
    search.add_indexer(Arc::new(NamedDocs(names.clone())));
    assert_eq!(total(&search, &stores, &lexicons), 2);

    *names.lock().unwrap() = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
    let served = std::cell::Cell::new(None);
    search
        .rebuild(&stores.indexer_ctx(), NonZeroUsize::new(1).unwrap(), &mut |_| {
            if served.get().is_none() {
                served.set(Some(
                    search
                        .search(&stores.ctx(&lexicons), SearchParams::default())
                        .unwrap()
                        .total,
                ));
            }
            ControlFlow::Continue(())
        })
        .unwrap();

    assert_eq!(served.get(), Some(2));
    assert_eq!(total(&search, &stores, &lexicons), 3);
}

#[test]
fn test_rebuild_derives_once_off_lock_when_no_mutation_races() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut search = SearchIndex::in_memory();
    search.add_indexer(Arc::new(CountingDocs {
        calls: calls.clone(),
        advance_serial: false,
        names: vec!["a", "b"],
    }));
    // A nonzero serial distinguishes the persisted frontier from an unwritten store.
    stores.meta.next_serial().unwrap();
    stores.meta.next_serial().unwrap();

    search
        .rebuild(&stores.indexer_ctx(), NonZeroUsize::new(4).unwrap(), &mut no_cancel)
        .unwrap();

    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "the derive ran once, off the writer lock"
    );
    assert_eq!(
        stores.meta.view_frontier(SEARCH_VIEW).unwrap(),
        Some(2),
        "the off-lock snapshot's serial is the one published"
    );
    assert_eq!(total(&search, &stores, &lexicons), 2);
}

#[test]
fn test_rebuild_re_derives_under_lock_when_a_mutation_advances_the_serial() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut search = SearchIndex::in_memory();
    search.add_indexer(Arc::new(CountingDocs {
        calls: calls.clone(),
        advance_serial: true,
        names: vec!["a", "b", "c"],
    }));

    search
        .rebuild(&stores.indexer_ctx(), NonZeroUsize::new(4).unwrap(), &mut no_cancel)
        .unwrap();

    assert_eq!(
        calls.load(Ordering::Relaxed),
        2,
        "the off-lock snapshot raced a serial bump, so the derive re-ran once under the lock"
    );
    assert_eq!(
        stores.meta.view_frontier(SEARCH_VIEW).unwrap(),
        Some(1),
        "the re-derived snapshot's serial is the one published"
    );
    assert_eq!(total(&search, &stores, &lexicons), 3);
}

fn artifact_doc(name: &str, text: &str) -> SearchDocument {
    SearchDocument {
        display_label: name.to_owned(),
        resource_key: name.to_owned(),
        route: "root".to_owned(),
        index: "root".to_owned(),
        ecosystem: "alpha".to_owned(),
        source: ContentSource::Cached,
        available_locally: false,
        summary: None,
        text: text.to_owned(),
    }
}

fn hits(search: &SearchIndex, stores: &Stores, lexicons: &LexiconRegistry, query: &str) -> usize {
    search
        .search(
            &stores.ctx(lexicons),
            SearchParams {
                query: query.to_owned(),
                ..SearchParams::default()
            },
        )
        .unwrap()
        .total
}

#[test]
fn test_update_resource_replaces_only_the_named_resource() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let mut search = SearchIndex::in_memory();
    search.add_indexer(Arc::new(NamedDocs(Arc::new(Mutex::new(vec![
        "alpha".to_owned(),
        "beta".to_owned(),
    ])))));
    assert_eq!(total(&search, &stores, &lexicons), 2);

    search
        .update_resource(
            &[artifact_doc("alpha", "alpha renamed")],
            &crate::document_key("root", "alpha"),
        )
        .unwrap();

    assert_eq!(
        hits(&search, &stores, &lexicons, "renamed"),
        1,
        "alpha reflects its new text"
    );
    assert_eq!(hits(&search, &stores, &lexicons, "beta"), 1, "beta is untouched");
    assert_eq!(
        total(&search, &stores, &lexicons),
        2,
        "no resource was added or dropped"
    );
}

#[test]
fn test_update_resource_retires_a_resource_given_no_documents() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let mut search = SearchIndex::in_memory();
    search.add_indexer(Arc::new(NamedDocs(Arc::new(Mutex::new(vec![
        "alpha".to_owned(),
        "beta".to_owned(),
    ])))));
    assert_eq!(total(&search, &stores, &lexicons), 2);

    search
        .update_resource(&[], &crate::document_key("root", "alpha"))
        .unwrap();

    assert_eq!(hits(&search, &stores, &lexicons, "alpha"), 0, "alpha was retired");
    assert_eq!(total(&search, &stores, &lexicons), 1, "only beta remains");
}

#[test]
fn test_update_resource_is_idempotent_across_a_repeated_apply() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let mut search = SearchIndex::in_memory();
    search.add_indexer(Arc::new(NamedDocs(Arc::new(Mutex::new(Vec::new())))));
    assert_eq!(total(&search, &stores, &lexicons), 0);
    search
        .update_resource(
            &[artifact_doc("alpha", "alpha old")],
            &crate::document_key("root", "alpha"),
        )
        .unwrap();

    for _ in 0..2 {
        search
            .update_resource(
                &[artifact_doc("alpha", "alpha new")],
                &crate::document_key("root", "alpha"),
            )
            .unwrap();
    }

    assert_eq!(
        (
            hits(&search, &stores, &lexicons, "old"),
            hits(&search, &stores, &lexicons, "new"),
            total(&search, &stores, &lexicons),
        ),
        (0, 1, 1)
    );
}

struct TrackingDocs {
    docs: Arc<Mutex<BTreeMap<String, String>>>,
    full: Arc<AtomicUsize>,
    scoped: Arc<Mutex<Vec<String>>>,
}

impl TrackingDocs {
    fn new(pairs: &[(&str, &str)]) -> Self {
        let docs = pairs
            .iter()
            .map(|(name, text)| ((*name).to_owned(), (*text).to_owned()))
            .collect();
        Self {
            docs: Arc::new(Mutex::new(docs)),
            full: Arc::new(AtomicUsize::new(0)),
            scoped: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn handle(&self) -> Self {
        Self {
            docs: self.docs.clone(),
            full: self.full.clone(),
            scoped: self.scoped.clone(),
        }
    }

    fn set(&self, pairs: &[(&str, &str)]) {
        *self.docs.lock().unwrap() = pairs
            .iter()
            .map(|(name, text)| ((*name).to_owned(), (*text).to_owned()))
            .collect();
    }
}

impl SearchDocumentProvider for TrackingDocs {
    fn documents(&self, _ctx: &IndexerCtx<'_>) -> Result<Vec<SearchDocument>, SearchError> {
        self.full.fetch_add(1, Ordering::Relaxed);
        Ok(self
            .docs
            .lock()
            .unwrap()
            .iter()
            .map(|(name, text)| artifact_doc(name, text))
            .collect())
    }

    fn resource_update(&self, _ctx: &IndexerCtx<'_>, name: &str) -> Result<ResourceUpdate, SearchError> {
        self.scoped.lock().unwrap().push(name.to_owned());
        let documents = self
            .docs
            .lock()
            .unwrap()
            .get(name)
            .map(|text| artifact_doc(name, text))
            .into_iter()
            .collect();
        Ok(ResourceUpdate {
            keys: vec![crate::document_key("root", name)],
            documents,
        })
    }
}

#[test]
fn test_lazy_refresh_rewrites_only_the_invalidated_resource() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let tracker = TrackingDocs::new(&[("alpha", "alpha one"), ("beta", "beta one")]);
    let mut search = SearchIndex::in_memory();
    search.add_indexer(Arc::new(tracker.handle()));
    assert_eq!(total(&search, &stores, &lexicons), 2);

    tracker.set(&[("alpha", "alpha two"), ("beta", "beta two")]);
    search.invalidate_resource("alpha");
    tracker.scoped.lock().unwrap().clear();

    assert_eq!(
        hits(&search, &stores, &lexicons, "two"),
        1,
        "only alpha re-derived to its new text"
    );
    assert_eq!(
        hits(&search, &stores, &lexicons, "beta two"),
        0,
        "beta was not re-derived"
    );
    assert_eq!(
        hits(&search, &stores, &lexicons, "beta one"),
        1,
        "beta keeps its prior text"
    );
    assert_eq!(
        *tracker.scoped.lock().unwrap(),
        vec!["alpha".to_owned()],
        "the refresh derived only the invalidated resource"
    );
}

#[test]
fn test_lazy_refresh_retires_a_deleted_resource() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let tracker = TrackingDocs::new(&[("alpha", "alpha one"), ("beta", "beta one")]);
    let mut search = SearchIndex::in_memory();
    search.add_indexer(Arc::new(tracker.handle()));
    assert_eq!(total(&search, &stores, &lexicons), 2);

    tracker.set(&[("beta", "beta one")]);
    search.invalidate_resource("alpha");

    assert_eq!(
        hits(&search, &stores, &lexicons, "alpha"),
        0,
        "the deleted resource is retired"
    );
    assert_eq!(total(&search, &stores, &lexicons), 1, "only beta remains");
}

#[test]
fn test_bump_epoch_re_derives_every_resource() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let tracker = TrackingDocs::new(&[("alpha", "alpha one"), ("beta", "beta one")]);
    let mut search = SearchIndex::in_memory();
    search.add_indexer(Arc::new(tracker.handle()));
    assert_eq!(total(&search, &stores, &lexicons), 2);

    tracker.set(&[("alpha", "alpha two"), ("beta", "beta two")]);
    search.bump_epoch();

    assert_eq!(
        hits(&search, &stores, &lexicons, "beta two"),
        1,
        "the blanket refresh re-derived beta"
    );
    assert!(
        tracker.scoped.lock().unwrap().is_empty(),
        "no resource was refreshed incrementally"
    );
    assert_eq!(
        tracker.full.load(Ordering::Relaxed),
        2,
        "the whole corpus was derived again"
    );
}

#[test]
fn test_invalidate_before_the_first_index_falls_back_to_a_full_build() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let tracker = TrackingDocs::new(&[("alpha", "alpha one"), ("beta", "beta one")]);
    let mut search = SearchIndex::in_memory();
    search.add_indexer(Arc::new(tracker.handle()));

    search.invalidate_resource("alpha");

    assert_eq!(
        total(&search, &stores, &lexicons),
        2,
        "the first build indexes every resource"
    );
    assert!(
        tracker.scoped.lock().unwrap().is_empty(),
        "no scoped derivation ran before the first build"
    );
    assert_eq!(tracker.full.load(Ordering::Relaxed), 1);
}

struct GatedDocs {
    text: Arc<Mutex<String>>,
    entered: Arc<Barrier>,
    release: Arc<Barrier>,
    gated: AtomicBool,
}

impl SearchDocumentProvider for GatedDocs {
    fn documents(&self, _ctx: &IndexerCtx<'_>) -> Result<Vec<SearchDocument>, SearchError> {
        Ok(vec![artifact_doc("pkg", &self.text.lock().unwrap())])
    }

    fn resource_update(&self, _ctx: &IndexerCtx<'_>, name: &str) -> Result<ResourceUpdate, SearchError> {
        let documents = (name == "pkg")
            .then(|| artifact_doc("pkg", &self.text.lock().unwrap()))
            .into_iter()
            .collect();
        if !self.gated.swap(true, Ordering::Relaxed) {
            self.entered.wait();
            self.release.wait();
        }
        Ok(ResourceUpdate {
            keys: vec![crate::document_key("root", name)],
            documents,
        })
    }
}

#[test]
fn test_search_stays_available_during_a_scoped_update() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let text = Arc::new(Mutex::new("stale".to_owned()));
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let mut search = SearchIndex::in_memory();
    search.add_indexer(Arc::new(GatedDocs {
        text: text.clone(),
        entered: entered.clone(),
        release: release.clone(),
        gated: AtomicBool::new(false),
    }));
    let search = Arc::new(search);
    assert_eq!(total(&search, &stores, &lexicons), 1);

    *text.lock().unwrap() = "fresh".to_owned();
    search.invalidate_resource("pkg");
    std::thread::scope(|scope| {
        let refresher = scope.spawn(|| hits(&search, &stores, &lexicons, "fresh"));
        entered.wait();
        let served_stale = hits(&search, &stores, &lexicons, "stale");
        let served_fresh = hits(&search, &stores, &lexicons, "fresh");
        release.wait();
        assert_eq!(refresher.join().unwrap(), 1, "the update publishes the fresh document");
        assert_eq!(served_stale, 1, "the prior index answers while the update runs");
        assert_eq!(served_fresh, 0, "the mid-flight update is not visible");
    });
    assert_eq!(
        hits(&search, &stores, &lexicons, "fresh"),
        1,
        "the published update is visible afterward"
    );
}

#[test]
fn test_invalidation_during_a_scoped_update_remains_pending() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let text = Arc::new(Mutex::new("initial".to_owned()));
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let mut search = SearchIndex::in_memory();
    search.add_indexer(Arc::new(GatedDocs {
        text: text.clone(),
        entered: entered.clone(),
        release: release.clone(),
        gated: AtomicBool::new(false),
    }));
    let search = Arc::new(search);
    assert_eq!(total(&search, &stores, &lexicons), 1);

    *text.lock().unwrap() = "first".to_owned();
    search.invalidate_resource("pkg");
    std::thread::scope(|scope| {
        let refresher = scope.spawn(|| hits(&search, &stores, &lexicons, "first"));
        entered.wait();
        *text.lock().unwrap() = "second".to_owned();
        search.invalidate_resource("pkg");
        release.wait();
        assert_eq!(refresher.join().unwrap(), 1);
    });

    assert_eq!(hits(&search, &stores, &lexicons, "second"), 1);
    assert_eq!(hits(&search, &stores, &lexicons, "first"), 0);
}

struct RacingDocs {
    docs: Arc<Mutex<BTreeMap<String, String>>>,
    full: Arc<AtomicUsize>,
    scoped: Arc<Mutex<Vec<String>>>,
    search: Arc<OnceLock<Weak<SearchIndex>>>,
    on_scoped: bool,
    fired: AtomicBool,
}

impl RacingDocs {
    fn fire(&self) {
        if !self.fired.swap(true, Ordering::Relaxed)
            && let Some(search) = self.search.get().and_then(Weak::upgrade)
        {
            search.bump_epoch();
        }
    }
}

impl SearchDocumentProvider for RacingDocs {
    fn documents(&self, _ctx: &IndexerCtx<'_>) -> Result<Vec<SearchDocument>, SearchError> {
        self.full.fetch_add(1, Ordering::Relaxed);
        if !self.on_scoped {
            self.fire();
        }
        Ok(self
            .docs
            .lock()
            .unwrap()
            .iter()
            .map(|(name, text)| artifact_doc(name, text))
            .collect())
    }

    fn resource_update(&self, _ctx: &IndexerCtx<'_>, name: &str) -> Result<ResourceUpdate, SearchError> {
        self.scoped.lock().unwrap().push(name.to_owned());
        if self.on_scoped {
            self.fire();
        }
        let documents = self
            .docs
            .lock()
            .unwrap()
            .get(name)
            .map(|text| artifact_doc(name, text))
            .into_iter()
            .collect();
        Ok(ResourceUpdate {
            keys: vec![crate::document_key("root", name)],
            documents,
        })
    }
}

fn racing_search(on_scoped: bool) -> (Arc<SearchIndex>, Arc<AtomicUsize>, Arc<Mutex<Vec<String>>>) {
    let full = Arc::new(AtomicUsize::new(0));
    let scoped = Arc::new(Mutex::new(Vec::new()));
    let slot = Arc::new(OnceLock::new());
    let mut search = SearchIndex::in_memory();
    search.add_indexer(Arc::new(RacingDocs {
        docs: Arc::new(Mutex::new(BTreeMap::from([(
            "alpha".to_owned(),
            "alpha one".to_owned(),
        )]))),
        full: full.clone(),
        scoped: scoped.clone(),
        search: slot.clone(),
        on_scoped,
        fired: AtomicBool::new(false),
    }));
    let search = Arc::new(search);
    slot.set(Arc::downgrade(&search)).ok();
    (search, full, scoped)
}

#[test]
fn test_blanket_invalidation_during_a_full_build_forces_another() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let (search, full, _scoped) = racing_search(false);

    assert_eq!(total(&search, &stores, &lexicons), 1);
    assert_eq!(total(&search, &stores, &lexicons), 1);
    assert_eq!(
        full.load(Ordering::Relaxed),
        2,
        "the raced build did not leave the index current"
    );
}

#[test]
fn test_blanket_invalidation_during_a_scoped_update_supersedes_it() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let (search, full, scoped) = racing_search(true);
    assert_eq!(total(&search, &stores, &lexicons), 1);

    search.invalidate_resource("alpha");
    assert_eq!(total(&search, &stores, &lexicons), 1);
    assert_eq!(total(&search, &stores, &lexicons), 1);
    assert_eq!(
        *scoped.lock().unwrap(),
        vec!["alpha".to_owned()],
        "the scoped path ran once"
    );
    assert_eq!(
        full.load(Ordering::Relaxed),
        2,
        "the raced scoped write was followed by a full rebuild"
    );
}
