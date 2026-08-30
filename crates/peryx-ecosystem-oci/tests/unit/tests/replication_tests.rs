use std::sync::Arc;

use axum::http::{Method, StatusCode};
use peryx_driver::AppState;
use peryx_ha::{ReplicaPage, ReplicaViewApplier as _};

use super::{app_with_journal, auth, hosted_writable, oci_digest, send, send_body, writable_index};

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
