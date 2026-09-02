use std::sync::Arc;

use axum::http::{Method, StatusCode};
use peryx_driver::AppState;
use peryx_ha::{ReplicaPage, ReplicaViewApplier as _};

use super::{app_with_journal, app_with_setup, auth, hosted_writable, oci_digest, send, send_body, writable_index};
use tempfile::TempDir;

const TOKEN: &str = "s3cret";

#[test]
fn replicated_registry_keys_do_not_block_the_shared_frontier() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _router) = hosted_writable(&dir, TOKEN);
    let key = crate::store::blob_membership_key("store", "app", "sha256:fixture");
    state.serving.meta.put_driver_value(&key, b"present").unwrap();

    state.apply(
        ReplicaPage {
            changes: 1,
            serial: 1,
            primary_serial: 1,
            revocations: Vec::new(),
        },
        &[key],
    );

    assert_eq!(
        state
            .serving
            .meta
            .view_frontier(peryx_driver::state::SEARCH_VIEW)
            .unwrap(),
        Some(1)
    );
}

fn replica(dir: &tempfile::TempDir) -> (Arc<AppState>, axum::Router) {
    app_with_journal(dir, vec![writable_index("store", "store", true, TOKEN)], true)
}

async fn push_blob(app: &axum::Router, repo: &str, blob: &[u8]) -> StatusCode {
    let digest = oci_digest(blob);
    send_body(
        app,
        Method::POST,
        &format!("/v2/{repo}/blobs/uploads/?digest={digest}"),
        &[("authorization", &auth(TOKEN))],
        blob.to_vec(),
    )
    .await
    .0
}

async fn pull_blob(app: &axum::Router, repo: &str, digest: &str) -> StatusCode {
    send(app, Method::GET, &format!("/v2/{repo}/blobs/{digest}")).await.0
}

/// Replay the home's membership removal the way the replica runtime does: commit the rows the primary
/// page carries, then hand the changed keys to the ecosystem drivers.
fn replay_removal(state: &AppState, repo: &str, digest: &str) {
    crate::quota::release_blob_membership(&state.serving.meta, "store", repo, digest, None, false).unwrap();
    replay_keys(state, &[crate::store::blob_membership_key("store", repo, digest)]);
}

fn replay_keys(state: &AppState, changed_keys: &[String]) {
    let serial = state.serving.meta.current_serial().unwrap();
    state.apply(
        ReplicaPage {
            changes: 1,
            serial,
            primary_serial: serial,
            revocations: Vec::new(),
        },
        changed_keys,
    );
}

#[tokio::test]
async fn test_a_replicated_removal_stops_the_replica_from_serving_the_blob() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = replica(&dir);
    let blob = b"a-replicated-layer";
    let digest = oci_digest(blob);
    assert_eq!(push_blob(&app, "store/app", blob).await, StatusCode::CREATED);
    assert_eq!(pull_blob(&app, "store/app", &digest).await, StatusCode::OK);

    replay_removal(&state, "app", &digest);

    assert_eq!(pull_blob(&app, "store/app", &digest).await, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_a_replayed_removal_leaves_the_blob_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = replica(&dir);
    let blob = b"a-twice-applied-layer";
    let digest = oci_digest(blob);
    assert_eq!(push_blob(&app, "store/app", blob).await, StatusCode::CREATED);
    assert_eq!(pull_blob(&app, "store/app", &digest).await, StatusCode::OK);

    replay_removal(&state, "app", &digest);
    replay_removal(&state, "app", &digest);

    assert_eq!(pull_blob(&app, "store/app", &digest).await, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_a_replicated_removal_keeps_another_repositorys_link() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = replica(&dir);
    let blob = b"a-shared-replicated-layer";
    let digest = oci_digest(blob);
    assert_eq!(push_blob(&app, "store/app", blob).await, StatusCode::CREATED);
    assert_eq!(push_blob(&app, "store/api", blob).await, StatusCode::CREATED);
    assert_eq!(pull_blob(&app, "store/api", &digest).await, StatusCode::OK);

    replay_removal(&state, "app", &digest);

    assert_eq!(pull_blob(&app, "store/api", &digest).await, StatusCode::OK);
}

#[tokio::test]
async fn test_a_replicated_change_outside_oci_membership_keeps_the_blob_served() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = replica(&dir);
    let blob = b"an-untouched-layer";
    let digest = oci_digest(blob);
    assert_eq!(push_blob(&app, "store/app", blob).await, StatusCode::CREATED);
    assert_eq!(pull_blob(&app, "store/app", &digest).await, StatusCode::OK);

    replay_keys(&state, &["pypi\u{0}p\u{0}store/demo".to_owned()]);

    assert_eq!(pull_blob(&app, "store/app", &digest).await, StatusCode::OK);
}

/// Counts the documents it builds, so a test can tell a page that re-derived one repository from one
/// that re-derived the store. It composes with the OCI indexer rather than replacing it, so the page
/// under test drives the real driver and this only records the work the refresh paid for.
struct CountingDocs {
    resources: Vec<String>,
    built: std::sync::atomic::AtomicUsize,
}

impl CountingDocs {
    fn new(count: usize) -> Self {
        Self {
            resources: (0..count).map(|position| format!("counted{position}")).collect(),
            built: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn built(&self) -> usize {
        self.built.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn document(&self, name: &str) -> peryx_search::SearchDocument {
        self.built.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        peryx_search::SearchDocument {
            display_label: name.to_owned(),
            resource_key: name.to_owned(),
            route: "counted".to_owned(),
            index: "counted".to_owned(),
            ecosystem: crate::ECOSYSTEM.as_str().to_owned(),
            source: peryx_search::ContentSource::Cached,
            available_locally: false,
            summary: None,
            text: name.to_owned(),
        }
    }
}

impl peryx_search::SearchDocumentProvider for CountingDocs {
    fn documents(
        &self,
        _ctx: &peryx_search::IndexerCtx<'_>,
    ) -> Result<Vec<peryx_search::SearchDocument>, peryx_search::SearchError> {
        Ok(self.resources.iter().map(|name| self.document(name)).collect())
    }

    fn resource_update(
        &self,
        _ctx: &peryx_search::IndexerCtx<'_>,
        name: &str,
    ) -> Result<peryx_search::ResourceUpdate, peryx_search::SearchError> {
        Ok(peryx_search::ResourceUpdate {
            keys: vec![peryx_search::document_key("counted", name)],
            documents: vec![self.document(name)],
        })
    }
}

/// A replica whose published search index already holds `count` counted documents, with the counter
/// zeroed so the next search reports only the work the applied page cost.
fn counted_replica(dir: &TempDir, count: usize) -> (Arc<AppState>, Arc<CountingDocs>) {
    let docs = Arc::new(CountingDocs::new(count));
    let counted = docs.clone();
    let (state, _router) = app_with_setup(
        dir,
        vec![writable_index("store", "store", true, TOKEN)],
        true,
        move |state| {
            Arc::get_mut(&mut state.serving)
                .expect("the serving state is still unique during the build")
                .search
                .add_indexer(counted);
        },
    );
    assert_eq!(search_hits(&state, ""), count);
    docs.built.store(0, std::sync::atomic::Ordering::Relaxed);
    (state, docs)
}

fn search_hits(state: &Arc<AppState>, query: &str) -> usize {
    state
        .serving
        .search
        .search(
            &state.search_ctx(),
            peryx_search::SearchParams {
                query: query.to_owned(),
                ..peryx_search::SearchParams::default()
            },
        )
        .unwrap()
        .total
}

/// A replicated tag write names its repository, so the refresh that follows derives that repository
/// alone instead of walking every document the replica stores. One construction is the scoped path
/// asking for one resource; eight is the fallback asking for the store.
#[test]
fn test_a_replicated_tag_page_re_derives_only_the_repository_it_named() {
    let dir = tempfile::tempdir().unwrap();
    let (state, docs) = counted_replica(&dir, 8);
    crate::store::put_tag(&state.serving.meta, "store", "app", "v1", "sha256:beef").unwrap();

    replay_keys(&state, &["oci\u{0}t\u{0}store\u{0}app\u{0}v1".to_owned()]);

    assert_eq!(search_hits(&state, "v1"), 1);
    assert_eq!(docs.built(), 1);
}

/// A manifest row is keyed by digest and no document derives from one, so a page carrying only manifest
/// rows costs the index nothing.
#[test]
fn test_a_replicated_manifest_row_re_derives_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (state, docs) = counted_replica(&dir, 8);

    replay_keys(&state, &["oci\u{0}m\u{0}sha256:beef".to_owned()]);

    assert_eq!(search_hits(&state, ""), 8);
    assert_eq!(docs.built(), 0);
}

/// An upload session is a replicated row the classification does not name, so it keeps the full
/// re-derivation rather than passing unreported work off as covered.
#[test]
fn test_an_unclassified_replicated_row_re_derives_every_document() {
    let dir = tempfile::tempdir().unwrap();
    let (state, docs) = counted_replica(&dir, 8);

    replay_keys(&state, &["oci/upload-session/7".to_owned()]);

    assert_eq!(search_hits(&state, ""), 8);
    assert_eq!(docs.built(), 8);
}

/// One unreported key is enough to owe the whole index a re-derivation: the repository the page also
/// named does not buy the rest of it a pass.
#[test]
fn test_a_page_mixing_a_tag_with_an_unclassified_row_re_derives_every_document() {
    let dir = tempfile::tempdir().unwrap();
    let (state, docs) = counted_replica(&dir, 8);
    crate::store::put_tag(&state.serving.meta, "store", "app", "v1", "sha256:beef").unwrap();

    replay_keys(
        &state,
        &[
            "oci\u{0}t\u{0}store\u{0}app\u{0}v1".to_owned(),
            "oci/upload-session/7".to_owned(),
        ],
    );

    assert_eq!(search_hits(&state, "v1"), 1);
    assert_eq!(docs.built(), 8);
}
