use peryx_driver::AppState;
use peryx_ha::{ReplicaPage, ReplicaViewApplier as _};
use peryx_identity::IndexAcl;
use peryx_index::{Index, IndexKind};
use peryx_policy::Policy;
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;

fn state() -> (tempfile::TempDir, std::sync::Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let mut state = AppState::new(
        MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        BlobStore::new(dir.path().join("blobs")),
        60,
        vec![Index {
            name: "hosted".to_owned(),
            route: "hosted".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Hosted { volatile: false },
            policy: Policy::default(),
            acl: IndexAcl::default(),
        }],
    );
    super::http::install_distributed_services(&mut state);
    let state = super::wired_distributed(state);
    super::http::initialize_distributed_schema(&state);
    (dir, state)
}

#[test]
fn replicated_project_change_retires_cached_pages() {
    let (_dir, state) = state();
    let before = state.serving.representation_key("hosted", "flask", "simple.html");
    state
        .serving
        .cache
        .store_hot(before.clone(), axum::body::Bytes::from_static(b"page"), i64::MAX);

    state.apply(
        ReplicaPage {
            changes: 1,
            serial: 1,
            primary_serial: 1,
            revocations: Vec::new(),
        },
        &["pypi\0p\0hosted/flask".to_owned()],
    );

    assert_ne!(
        state.serving.representation_key("hosted", "flask", "simple.html"),
        before
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

#[test]
fn invalid_replicated_upload_holds_the_search_frontier() {
    let (_dir, state) = state();
    crate::store::put_upload(
        &state.serving.meta,
        "hosted",
        "flask",
        "flask-1.0-py3-none-any.whl",
        b"not json",
    )
    .unwrap();

    state.apply(
        ReplicaPage {
            changes: 1,
            serial: 1,
            primary_serial: 1,
            revocations: Vec::new(),
        },
        &["pypi\0u\0hosted/flask/flask-1.0-py3-none-any.whl".to_owned()],
    );

    assert_eq!(
        state
            .serving
            .meta
            .view_frontier(peryx_driver::state::SEARCH_VIEW)
            .unwrap(),
        None
    );
}
