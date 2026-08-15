use axum::extract::FromRef as _;

use super::*;

#[test]
fn ui_state_projects_leptos_options() {
    let directory = tempfile::tempdir().unwrap();
    let state = UiState {
        options: leptos_options(),
        app: Arc::new(AppState::new(
            peryx_storage::meta::MetaStore::open(directory.path().join("peryx.redb")).unwrap(),
            peryx_storage::blob::BlobStore::new(directory.path().join("blobs")),
            60,
            Vec::new(),
        )),
    };

    assert_eq!(LeptosOptions::from_ref(&state).site_root, state.options.site_root);
}
