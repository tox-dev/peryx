use std::collections::BTreeSet;
use std::str::FromStr as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use peryx_core::{Ecosystem, Lexicon};
use peryx_ha::{ReplicaPage, ReplicaViewApplier as _};
use peryx_identity::{ArtifactDigest, DigestDecision, RevocationReason, UserId};
use peryx_search::{
    ContentSource, IndexerCtx, ResourceUpdate, SearchDocument, SearchDocumentProvider, SearchError, SearchParams,
    document_key,
};
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;

use super::AppState;

struct BlockedView;

impl crate::serving::ReplicatedApplyDriver for BlockedView {
    fn apply_replicated_changes<'key>(
        &self,
        _state: &crate::ServingState,
        _changed_keys: &'key [String],
    ) -> Result<BTreeSet<&'key str>, crate::state::ViewBlock> {
        Err(crate::state::ViewBlock {
            view: "search".to_owned(),
        })
    }
}

fn state() -> (tempfile::TempDir, AppState, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    meta.initialize_distributed_state().unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let state = AppState::new(meta.clone(), blobs, 60, Vec::new());
    (dir, state, meta)
}

#[test]
fn test_empty_replica_page_changes_nothing() {
    let (_dir, state, _meta) = state();

    state.apply(
        ReplicaPage {
            changes: 0,
            serial: 1,
            primary_serial: 1,
            revocations: Vec::new(),
        },
        &[],
    );

    assert_eq!(peryx_ha::ReplicaViewApplier::readable_frontier(&state), 0);
}

#[test]
fn test_replica_page_advances_search_view() {
    let (_dir, state, meta) = state();

    state.apply(
        ReplicaPage {
            changes: 1,
            serial: 1,
            primary_serial: 1,
            revocations: Vec::new(),
        },
        &["resource".to_owned()],
    );

    assert_eq!(meta.view_frontiers().unwrap().get(crate::state::SEARCH_VIEW), Some(&1));
}

#[test]
fn test_replica_page_advances_the_readable_frontier() {
    let (_dir, state, meta) = state();
    meta.next_serial().unwrap();

    state.apply(
        ReplicaPage {
            changes: 1,
            serial: 1,
            primary_serial: 1,
            revocations: Vec::new(),
        },
        &["resource".to_owned()],
    );

    assert_eq!(peryx_ha::ReplicaViewApplier::readable_frontier(&state), 1);
}

#[test]
fn test_blocked_replica_view_does_not_advance_the_frontier() {
    let (_dir, mut state, meta) = state();
    state.register_replicated_apply_driver(Ecosystem::new("example"), Arc::new(BlockedView));

    state.apply(
        ReplicaPage {
            changes: 1,
            serial: 1,
            primary_serial: 1,
            revocations: Vec::new(),
        },
        &["resource".to_owned()],
    );

    assert!(meta.view_frontiers().unwrap().is_empty());
}

#[test]
fn test_replica_apply_surfaces_a_frontier_write_failure_without_advancing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let meta = MetaStore::open(&path).unwrap();
    meta.initialize_distributed_state().unwrap();
    drop(meta);
    let state = AppState::new(
        MetaStore::open_existing_read_only(&path).unwrap(),
        BlobStore::new(dir.path().join("blobs")),
        60,
        Vec::new(),
    );

    state.apply(
        ReplicaPage {
            changes: 1,
            serial: 1,
            primary_serial: 1,
            revocations: Vec::new(),
        },
        &[],
    );

    assert!(
        MetaStore::open_existing_read_only(&path)
            .unwrap()
            .view_frontiers()
            .unwrap()
            .is_empty()
    );
}

fn revoked_digest(meta: &MetaStore) -> ArtifactDigest {
    let digest = ArtifactDigest::from_str(&format!("sha256:{:064x}", 1)).unwrap();
    meta.put_digest_revocation(
        &digest,
        &RevocationReason::new("incident").unwrap(),
        &UserId::random(),
        10,
    )
    .unwrap();
    digest
}

/// The replica commits the row before the page reaches the applier, so the cached decision the node
/// formed under the old rows is what stands between a follower and the bytes the writer revoked.
#[test]
fn test_replica_page_retires_the_decision_it_revoked() {
    let (_dir, state, meta) = state();
    let digest = ArtifactDigest::from_str(&format!("sha256:{:064x}", 1)).unwrap();
    assert_eq!(
        state.serving.revocations.decision(&digest).unwrap(),
        DigestDecision::Clear
    );
    revoked_digest(&meta);

    state.apply(
        ReplicaPage {
            changes: 1,
            serial: 1,
            primary_serial: 1,
            revocations: vec![digest.clone()],
        },
        &[],
    );

    assert_eq!(
        state.serving.revocations.decision(&digest).unwrap(),
        DigestDecision::Revoked
    );
}

/// Every page would otherwise pay a full decision-cache flush on the download path.
#[test]
fn test_replica_page_without_revocations_keeps_the_decision_cache() {
    let (_dir, state, meta) = state();
    let digest = revoked_digest(&meta);
    assert_eq!(
        state.serving.revocations.decision(&digest).unwrap(),
        DigestDecision::Revoked
    );
    meta.lift_digest_revocation(&digest, &UserId::random(), 11).unwrap();

    state.apply(
        ReplicaPage {
            changes: 1,
            serial: 1,
            primary_serial: 1,
            revocations: Vec::new(),
        },
        &[],
    );

    assert_eq!(
        state.serving.revocations.decision(&digest).unwrap(),
        DigestDecision::Revoked
    );
}

const INDEXED_ROUTE: &str = "root";

/// Counts the documents it builds, so a test can tell a page that rebuilt one resource from one that
/// re-derived the store.
struct CountingDocs {
    resources: Vec<String>,
    built: AtomicUsize,
}

impl CountingDocs {
    fn new(resources: &[&str]) -> Self {
        Self {
            resources: resources.iter().map(|&name| name.to_owned()).collect(),
            built: AtomicUsize::new(0),
        }
    }

    fn document(&self, name: &str) -> SearchDocument {
        self.built.fetch_add(1, Ordering::Relaxed);
        SearchDocument {
            display_label: name.to_owned(),
            resource_key: name.to_owned(),
            route: INDEXED_ROUTE.to_owned(),
            index: INDEXED_ROUTE.to_owned(),
            ecosystem: "indexed".to_owned(),
            source: ContentSource::Cached,
            available_locally: false,
            summary: None,
            text: name.to_owned(),
        }
    }
}

impl SearchDocumentProvider for CountingDocs {
    fn documents(&self, _ctx: &IndexerCtx<'_>) -> Result<Vec<SearchDocument>, SearchError> {
        Ok(self.resources.iter().map(|name| self.document(name)).collect())
    }

    fn resource_update(&self, _ctx: &IndexerCtx<'_>, name: &str) -> Result<ResourceUpdate, SearchError> {
        Ok(ResourceUpdate {
            keys: vec![document_key(INDEXED_ROUTE, name)],
            documents: vec![self.document(name)],
        })
    }
}

/// Retires the resources it names and reports exactly the keys it covered, the way an ecosystem driver
/// reports the projects it rebuilt.
struct CoveringDriver {
    covered: Vec<String>,
}

impl CoveringDriver {
    fn new(covered: &[&str]) -> Self {
        Self {
            covered: covered.iter().map(|&key| key.to_owned()).collect(),
        }
    }
}

impl crate::serving::ReplicatedApplyDriver for CoveringDriver {
    fn apply_replicated_changes<'key>(
        &self,
        state: &crate::ServingState,
        changed_keys: &'key [String],
    ) -> Result<BTreeSet<&'key str>, crate::state::ViewBlock> {
        for key in &self.covered {
            state.invalidate_search_resource(key);
        }
        Ok(changed_keys
            .iter()
            .filter(|key| self.covered.contains(key))
            .map(String::as_str)
            .collect())
    }
}

/// Builds a searchable replica whose index is already published, and returns the document counter.
fn indexed_state(
    resources: &[&str],
    driver: Option<CoveringDriver>,
) -> (tempfile::TempDir, Arc<AppState>, Arc<CountingDocs>) {
    let (dir, mut state, _meta) = state();
    let docs = Arc::new(CountingDocs::new(resources));
    state.register_lexicon(Ecosystem::new("indexed"), &Lexicon::NEUTRAL);
    Arc::get_mut(&mut state.serving)
        .expect("the serving state is still unique during the build")
        .search
        .add_indexer(docs.clone());
    if let Some(driver) = driver {
        state.register_replicated_apply_driver(Ecosystem::new("indexed"), Arc::new(driver));
    }
    let state = Arc::new(state);
    assert_eq!(published_documents(&state), resources.len());
    docs.built.store(0, Ordering::Relaxed);
    (dir, state, docs)
}

/// Searches the replica and reports how many documents the published index answers with, so a test
/// pins the served index as well as the work its refresh paid for.
fn published_documents(state: &Arc<AppState>) -> usize {
    let services = crate::http_services::HttpDomainServices::for_state(state);
    services.search().search(SearchParams::default(), None).unwrap().total
}

fn changed_page(revocations: Vec<ArtifactDigest>) -> ReplicaPage {
    ReplicaPage {
        changes: 1,
        serial: 1,
        primary_serial: 1,
        revocations,
    }
}

/// The scoped refresh a driver set up must survive the apply that follows it: the neutral fallback
/// re-derives every document in the store, on an index whose changed document is already correct.
#[test]
fn test_page_a_driver_covered_rebuilds_only_the_changed_resource() {
    let (_dir, state, docs) = indexed_state(&["alpha", "beta", "gamma"], Some(CoveringDriver::new(&["alpha"])));

    state.apply(changed_page(Vec::new()), &["alpha".to_owned()]);

    assert_eq!(published_documents(&state), 3);
    assert_eq!(docs.built.load(Ordering::Relaxed), 1);
}

#[test]
fn test_page_a_driver_left_a_key_re_derives_every_document() {
    let (_dir, state, docs) = indexed_state(&["alpha", "beta", "gamma"], Some(CoveringDriver::new(&["alpha"])));

    state.apply(changed_page(Vec::new()), &["alpha".to_owned(), "beta".to_owned()]);

    assert_eq!(published_documents(&state), 3);
    assert_eq!(docs.built.load(Ordering::Relaxed), 3);
}

#[test]
fn test_page_without_an_apply_driver_re_derives_every_document() {
    let (_dir, state, docs) = indexed_state(&["alpha", "beta", "gamma"], None);

    state.apply(changed_page(Vec::new()), &["alpha".to_owned()]);

    assert_eq!(published_documents(&state), 3);
    assert_eq!(docs.built.load(Ordering::Relaxed), 3);
}

/// A revocation names a digest, and no index maps one back to the projects that publish it, so a
/// covered page still owes the whole view a re-derivation.
#[test]
fn test_page_carrying_a_revocation_re_derives_every_document() {
    let (_dir, state, docs) = indexed_state(&["alpha", "beta", "gamma"], Some(CoveringDriver::new(&["alpha"])));
    let digest = ArtifactDigest::from_str(&format!("sha256:{:064x}", 2)).unwrap();

    state.apply(changed_page(vec![digest]), &["alpha".to_owned()]);

    assert_eq!(published_documents(&state), 3);
    assert_eq!(docs.built.load(Ordering::Relaxed), 3);
}
