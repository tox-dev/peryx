use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex, OnceLock, Weak};

use peryx_core::LexiconRegistry;

use super::Stores;
use crate::context::IndexerCtx;
use crate::engine::{RebuildOutcome, RebuildProgress};
use crate::{
    PackageDocument, PackageIndexer, PackageSearch, PackageSource, ProjectUpdate, SEARCH_VIEW, SearchError,
    SearchParams,
};

/// A test indexer whose document set the test mutates between refreshes, so a rebuild can be shown to
/// publish content a lazy refresh would not yet pick up.
struct NamedDocs(Arc<Mutex<Vec<String>>>);

impl PackageIndexer for NamedDocs {
    fn documents(&self, _ctx: &IndexerCtx<'_>) -> Result<Vec<PackageDocument>, SearchError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|name| PackageDocument {
                display_name: name.clone(),
                normalized_name: name.clone(),
                route: "root".to_owned(),
                index: "root".to_owned(),
                ecosystem: "alpha".to_owned(),
                source: PackageSource::Cached,
                available_locally: false,
                summary: None,
                text: name.clone(),
            })
            .collect())
    }
}

/// Counts each derive and, when asked, advances the store serial as it derives, so a rebuild's off-lock
/// snapshot is seen to race a concurrent mutation and re-derive under the lock.
struct CountingDocs {
    calls: Arc<AtomicUsize>,
    advance_serial: bool,
    names: Vec<&'static str>,
}

impl PackageIndexer for CountingDocs {
    fn documents(&self, ctx: &IndexerCtx<'_>) -> Result<Vec<PackageDocument>, SearchError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if self.advance_serial {
            ctx.meta.next_serial().unwrap();
        }
        Ok(self.names.iter().map(|name| artifact_doc(name, name)).collect())
    }
}

fn total(search: &PackageSearch, stores: &Stores, lexicons: &LexiconRegistry) -> usize {
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
    // Leave an index a prior peryx built with a different schema.
    let mut legacy = tantivy::schema::Schema::builder();
    legacy.add_text_field("legacy", tantivy::schema::TEXT);
    tantivy::Index::builder()
        .schema(legacy.build())
        .create_in_dir(dir.path())
        .expect("create the legacy index");
    // Opening discards the mismatched index and rebuilds in place instead of failing startup.
    crate::PackageSearch::open(dir.path()).expect("open rebuilds a mismatched index");
}

#[test]
fn test_rebuild_publishes_new_documents_without_an_epoch_bump() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let names = Arc::new(Mutex::new(vec!["x".to_owned()]));
    let mut search = PackageSearch::in_memory();
    search.add_indexer(Arc::new(NamedDocs(names.clone())));
    assert_eq!(total(&search, &stores, &lexicons), 1);

    // A lazy refresh only reacts to an epoch bump; with none, the eager rebuild is what publishes.
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
    let mut search = PackageSearch::in_memory();
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
    let mut search = PackageSearch::in_memory();
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
    let mut search = PackageSearch::in_memory();
    search.add_indexer(Arc::new(NamedDocs(names.clone())));
    assert_eq!(total(&search, &stores, &lexicons), 2);

    // Stage one chunk, then cancel: the reader must still serve the two prior documents, never the
    // single staged one from the rolled-back candidate generation.
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
    let mut search = PackageSearch::in_memory();
    search.add_indexer(Arc::new(NamedDocs(names.clone())));
    assert_eq!(total(&search, &stores, &lexicons), 2);

    // Cancel a rebuild after the chunk holding `a` stages, then scope-update `x`. The reload that update
    // runs must not publish the abandoned candidate generation: `y` stays and `a`/`b`/`c` never surface.
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
        .update_project(&[artifact_doc("x", "x")], &crate::project_key("root", "x"))
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
        "only the prior two projects remain"
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
    let mut search = PackageSearch::open(&path).unwrap();
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
fn test_interrupted_on_disk_rebuild_leaves_a_marker_that_open_discards() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("search");
    let marker = Path::new(&path).with_extension("rebuilding");
    let stores = Stores::open(&dir);
    let names = Arc::new(Mutex::new(vec!["a".to_owned()]));
    let mut search = PackageSearch::open(&path).unwrap();
    search.add_indexer(Arc::new(NamedDocs(names)));

    let outcome = search
        .rebuild(&stores.indexer_ctx(), NonZeroUsize::new(1).unwrap(), &mut |_| {
            ControlFlow::Break(())
        })
        .unwrap();
    assert_eq!(outcome, RebuildOutcome::Aborted { documents: 0 });
    assert!(marker.exists(), "an interrupted rebuild leaves its marker");

    drop(search);
    PackageSearch::open(&path).unwrap();
    assert!(
        !marker.exists(),
        "reopening discards the partial index and clears the marker"
    );
}

#[test]
fn test_search_during_a_rebuild_serves_the_prior_index() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let names = Arc::new(Mutex::new(vec!["x".to_owned(), "y".to_owned()]));
    let mut search = PackageSearch::in_memory();
    search.add_indexer(Arc::new(NamedDocs(names.clone())));
    assert_eq!(total(&search, &stores, &lexicons), 2);

    // A search issued while the rebuild holds the lock skips the refresh and serves the two prior
    // documents rather than blocking or seeing the half-built three.
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
    let mut search = PackageSearch::in_memory();
    search.add_indexer(Arc::new(CountingDocs {
        calls: calls.clone(),
        advance_serial: false,
        names: vec!["a", "b"],
    }));
    // A non-empty serial to publish, so the persisted frontier proves the snapshot's serial rather than
    // the empty default an unwritten store shares with it.
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
    let mut search = PackageSearch::in_memory();
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

fn artifact_doc(name: &str, text: &str) -> PackageDocument {
    PackageDocument {
        display_name: name.to_owned(),
        normalized_name: name.to_owned(),
        route: "root".to_owned(),
        index: "root".to_owned(),
        ecosystem: "alpha".to_owned(),
        source: PackageSource::Cached,
        available_locally: false,
        summary: None,
        text: text.to_owned(),
    }
}

fn hits(search: &PackageSearch, stores: &Stores, lexicons: &LexiconRegistry, query: &str) -> usize {
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
fn test_update_project_replaces_only_the_named_project() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let mut search = PackageSearch::in_memory();
    search.add_indexer(Arc::new(NamedDocs(Arc::new(Mutex::new(vec![
        "alpha".to_owned(),
        "beta".to_owned(),
    ])))));
    // Build the index once; a scoped update never bumps the epoch, so it alone changes what follows.
    assert_eq!(total(&search, &stores, &lexicons), 2);

    search
        .update_project(
            &[artifact_doc("alpha", "alpha renamed")],
            &crate::project_key("root", "alpha"),
        )
        .unwrap();

    assert_eq!(
        hits(&search, &stores, &lexicons, "renamed"),
        1,
        "alpha reflects its new text"
    );
    assert_eq!(hits(&search, &stores, &lexicons, "beta"), 1, "beta is untouched");
    assert_eq!(total(&search, &stores, &lexicons), 2, "no project was added or dropped");
}

#[test]
fn test_update_project_retires_a_project_given_no_documents() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let mut search = PackageSearch::in_memory();
    search.add_indexer(Arc::new(NamedDocs(Arc::new(Mutex::new(vec![
        "alpha".to_owned(),
        "beta".to_owned(),
    ])))));
    assert_eq!(total(&search, &stores, &lexicons), 2);

    search
        .update_project(&[], &crate::project_key("root", "alpha"))
        .unwrap();

    assert_eq!(hits(&search, &stores, &lexicons, "alpha"), 0, "alpha was retired");
    assert_eq!(total(&search, &stores, &lexicons), 1, "only beta remains");
}

#[test]
fn test_update_project_is_idempotent_across_a_repeated_apply() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let mut search = PackageSearch::in_memory();
    search.add_indexer(Arc::new(NamedDocs(Arc::new(Mutex::new(vec!["alpha".to_owned()])))));
    assert_eq!(total(&search, &stores, &lexicons), 1);

    // Re-running the same delete-then-add, as a crash-recovery replay does, reaches the same index.
    for _ in 0..2 {
        search
            .update_project(&[artifact_doc("alpha", "alpha")], &crate::project_key("root", "alpha"))
            .unwrap();
    }

    assert_eq!(
        total(&search, &stores, &lexicons),
        1,
        "the project is present exactly once"
    );
}

/// A mutable corpus that records how each project reached the index: `full` counts whole-corpus
/// derivations, `scoped` records the project name behind each incremental one, so a test can prove a
/// mutation refreshed only its own project instead of rebuilding everything.
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

impl PackageIndexer for TrackingDocs {
    fn documents(&self, _ctx: &IndexerCtx<'_>) -> Result<Vec<PackageDocument>, SearchError> {
        self.full.fetch_add(1, Ordering::Relaxed);
        Ok(self
            .docs
            .lock()
            .unwrap()
            .iter()
            .map(|(name, text)| artifact_doc(name, text))
            .collect())
    }

    fn project_update(&self, _ctx: &IndexerCtx<'_>, name: &str) -> Result<ProjectUpdate, SearchError> {
        self.scoped.lock().unwrap().push(name.to_owned());
        let documents = self
            .docs
            .lock()
            .unwrap()
            .get(name)
            .map(|text| artifact_doc(name, text))
            .into_iter()
            .collect();
        Ok(ProjectUpdate {
            keys: vec![crate::project_key("root", name)],
            documents,
        })
    }
}

#[test]
fn test_lazy_refresh_rewrites_only_the_invalidated_project() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let tracker = TrackingDocs::new(&[("alpha", "alpha one"), ("beta", "beta one")]);
    let mut search = PackageSearch::in_memory();
    search.add_indexer(Arc::new(tracker.handle()));
    assert_eq!(total(&search, &stores, &lexicons), 2);

    // Both projects change, but only alpha is invalidated: beta must keep its prior text.
    tracker.set(&[("alpha", "alpha two"), ("beta", "beta two")]);
    search.invalidate_project("alpha");
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
        "the refresh derived only the invalidated project"
    );
}

#[test]
fn test_lazy_refresh_retires_a_deleted_project() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let tracker = TrackingDocs::new(&[("alpha", "alpha one"), ("beta", "beta one")]);
    let mut search = PackageSearch::in_memory();
    search.add_indexer(Arc::new(tracker.handle()));
    assert_eq!(total(&search, &stores, &lexicons), 2);

    tracker.set(&[("beta", "beta one")]);
    search.invalidate_project("alpha");

    assert_eq!(
        hits(&search, &stores, &lexicons, "alpha"),
        0,
        "the deleted project is retired"
    );
    assert_eq!(total(&search, &stores, &lexicons), 1, "only beta remains");
}

#[test]
fn test_bump_epoch_re_derives_every_project() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let tracker = TrackingDocs::new(&[("alpha", "alpha one"), ("beta", "beta one")]);
    let mut search = PackageSearch::in_memory();
    search.add_indexer(Arc::new(tracker.handle()));
    assert_eq!(total(&search, &stores, &lexicons), 2);

    // A blanket bump re-derives the whole corpus, so an unrelated project reflects its new text too.
    tracker.set(&[("alpha", "alpha two"), ("beta", "beta two")]);
    search.bump_epoch();

    assert_eq!(
        hits(&search, &stores, &lexicons, "beta two"),
        1,
        "the blanket refresh re-derived beta"
    );
    assert!(
        tracker.scoped.lock().unwrap().is_empty(),
        "no project was refreshed incrementally"
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
    let mut search = PackageSearch::in_memory();
    search.add_indexer(Arc::new(tracker.handle()));

    // With no index yet, a per-project invalidation cannot scope to one document, so the first search
    // builds the whole corpus rather than dropping the unnamed projects.
    search.invalidate_project("alpha");

    assert_eq!(
        total(&search, &stores, &lexicons),
        2,
        "the first build indexes every project"
    );
    assert!(
        tracker.scoped.lock().unwrap().is_empty(),
        "no scoped derivation ran before the first build"
    );
    assert_eq!(tracker.full.load(Ordering::Relaxed), 1);
}

/// A corpus with one document whose derivation blocks on two barriers, so a test can hold the writer
/// lock at a known point and search from another thread while the update is mid-flight.
struct GatedDocs {
    text: Arc<Mutex<String>>,
    entered: Arc<Barrier>,
    release: Arc<Barrier>,
}

impl PackageIndexer for GatedDocs {
    fn documents(&self, _ctx: &IndexerCtx<'_>) -> Result<Vec<PackageDocument>, SearchError> {
        Ok(vec![artifact_doc("pkg", &self.text.lock().unwrap())])
    }

    fn project_update(&self, _ctx: &IndexerCtx<'_>, name: &str) -> Result<ProjectUpdate, SearchError> {
        self.entered.wait();
        self.release.wait();
        let documents = (name == "pkg")
            .then(|| artifact_doc("pkg", &self.text.lock().unwrap()))
            .into_iter()
            .collect();
        Ok(ProjectUpdate {
            keys: vec![crate::project_key("root", name)],
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
    let mut search = PackageSearch::in_memory();
    search.add_indexer(Arc::new(GatedDocs {
        text: text.clone(),
        entered: entered.clone(),
        release: release.clone(),
    }));
    let search = Arc::new(search);
    assert_eq!(total(&search, &stores, &lexicons), 1);

    *text.lock().unwrap() = "fresh".to_owned();
    search.invalidate_project("pkg");
    std::thread::scope(|scope| {
        let refresher = scope.spawn(|| hits(&search, &stores, &lexicons, "fresh"));
        // The refresher now holds the writer lock inside the gated derivation; a search here must serve
        // the prior committed index rather than block on it or see the half-applied update.
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

/// A corpus that fires one blanket invalidation from inside a derivation, so a test can drive the race
/// where a mutation lands while a refresh is already writing. `on_scoped` fires from the incremental
/// path, otherwise it fires from the full one.
struct RacingDocs {
    docs: Arc<Mutex<BTreeMap<String, String>>>,
    full: Arc<AtomicUsize>,
    scoped: Arc<Mutex<Vec<String>>>,
    search: Arc<OnceLock<Weak<PackageSearch>>>,
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

impl PackageIndexer for RacingDocs {
    fn documents(&self, _ctx: &IndexerCtx<'_>) -> Result<Vec<PackageDocument>, SearchError> {
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

    fn project_update(&self, _ctx: &IndexerCtx<'_>, name: &str) -> Result<ProjectUpdate, SearchError> {
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
        Ok(ProjectUpdate {
            keys: vec![crate::project_key("root", name)],
            documents,
        })
    }
}

fn racing_search(on_scoped: bool) -> (Arc<PackageSearch>, Arc<AtomicUsize>, Arc<Mutex<Vec<String>>>) {
    let full = Arc::new(AtomicUsize::new(0));
    let scoped = Arc::new(Mutex::new(Vec::new()));
    let slot = Arc::new(OnceLock::new());
    let mut search = PackageSearch::in_memory();
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

    // The first search's full build bumps the epoch mid-derive, so it cannot be trusted current; a
    // second search must run the whole build again rather than mark the raced result as up to date.
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

    // A scoped refresh that a blanket bump races must not clear the pending full rebuild; the next search
    // rebuilds the whole corpus rather than trusting the superseded scoped write.
    search.invalidate_project("alpha");
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
