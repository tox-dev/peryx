//! A blob push retains a durable intent between committing its bytes and publishing its repository
//! membership, so an upload the authority turns away mid-flight stays finalizable at the home instead
//! of vanishing with the request. These drive both directions through the registry's own surface: a
//! retained push is not servable before the home finalizes it, and is servable after.

use std::sync::Arc;

use axum::http::{Method, StatusCode, header};
use peryx_driver::AppState;
use peryx_storage::meta::{IntentAdmission, IntentLimits, IntentPhase};

use super::{
    EpochAuthority, auth, bind_ownership, body_has_code, hosted_writable_distributed, oci_digest, send, send_body,
    send_with,
};

const TOKEN: &str = "s3cret";
const LAYER: &[u8] = b"a-layer-that-outlives-its-request";

fn intent_key(digest: &str) -> String {
    format!("oci:blob:store:app:{digest}")
}

async fn push_blob(app: &axum::Router, digest: &str) -> StatusCode {
    send_body(
        app,
        Method::POST,
        &format!("/v2/store/app/blobs/uploads/?digest={digest}"),
        &[("authorization", &auth(TOKEN))],
        LAYER.to_vec(),
    )
    .await
    .0
}

async fn pull_blob(app: &axum::Router, digest: &str) -> StatusCode {
    send(app, Method::GET, &format!("/v2/store/app/blobs/{digest}")).await.0
}

async fn finalize(state: &Arc<AppState>) -> u64 {
    let (_, finalizer) = state.intent_finalizers().next().expect("oci registers a finalizer");
    finalizer.finalize_admitted(state.serving.clone()).await
}

/// Publish one retained write the way an operator drain reaches it, through the registered capability
/// rather than the module behind it.
async fn drain_one(state: &Arc<AppState>, authority: &str, intent_key: &str) -> bool {
    let (_, finalizer) = state.intent_finalizers().next().expect("oci registers a finalizer");
    finalizer
        .finalize_retained(state.serving.clone(), authority, intent_key)
        .await
}

fn retained(state: &AppState, digest: &str) -> Option<IntentPhase> {
    state
        .serving
        .meta
        .staged_intent(&intent_key(digest))
        .unwrap()
        .map(|intent| intent.phase)
}

/// A push whose repository authority moved between the epoch it leased and the metadata commit. Its
/// bytes are durable and its intent retained, but nothing published.
async fn fenced_push(dir: &tempfile::TempDir) -> (Arc<AppState>, axum::Router, Arc<EpochAuthority>, String) {
    let (state, app) = hosted_writable_distributed(dir, TOKEN);
    let group = EpochAuthority::superseded(5, 6);
    bind_ownership(&state, group.clone());
    let digest = oci_digest(LAYER);
    assert_eq!(push_blob(&app, &digest).await, StatusCode::SERVICE_UNAVAILABLE);
    (state, app, group, digest)
}

#[tokio::test]
async fn test_a_fenced_push_answers_a_retryable_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable_distributed(&dir, TOKEN);
    bind_ownership(&state, EpochAuthority::superseded(5, 6));
    let digest = oci_digest(LAYER);

    let (status, _, body) = send_body(
        &app,
        Method::POST,
        &format!("/v2/store/app/blobs/uploads/?digest={digest}"),
        &[("authorization", &auth(TOKEN))],
        LAYER.to_vec(),
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body_has_code(&body, "UNAVAILABLE"));
}

#[tokio::test]
async fn test_a_fenced_push_is_retained_for_its_home() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _app, _group, digest) = fenced_push(&dir).await;

    assert_eq!(retained(&state, &digest), Some(IntentPhase::Pending));
}

#[tokio::test]
async fn test_a_retained_push_is_not_servable_before_its_home_finalizes_it() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app, _group, digest) = fenced_push(&dir).await;

    assert_eq!(pull_blob(&app, &digest).await, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_a_retained_push_is_servable_after_its_home_finalizes_it() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app, group, digest) = fenced_push(&dir).await;

    group.settle();
    let finalized = finalize(&state).await;

    assert_eq!((finalized, pull_blob(&app, &digest).await), (1, StatusCode::OK));
}

#[tokio::test]
async fn test_home_finalization_settles_the_retained_intent() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _app, group, digest) = fenced_push(&dir).await;

    group.settle();
    finalize(&state).await;

    assert_eq!(retained(&state, &digest), Some(IntentPhase::Admitted));
}

#[tokio::test]
async fn test_a_retained_push_stays_retained_while_the_authority_is_still_fenced() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app, _group, digest) = fenced_push(&dir).await;

    let finalized = finalize(&state).await;

    assert_eq!(
        (finalized, pull_blob(&app, &digest).await, retained(&state, &digest)),
        (0, StatusCode::NOT_FOUND, Some(IntentPhase::Pending))
    );
}

#[tokio::test]
async fn test_a_retained_push_stays_retained_while_the_home_is_unresolved() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app, _group, digest) = fenced_push(&dir).await;
    bind_ownership(&state, EpochAuthority::unavailable(6));

    let finalized = finalize(&state).await;

    assert_eq!((finalized, pull_blob(&app, &digest).await), (0, StatusCode::NOT_FOUND));
}

#[tokio::test]
async fn test_a_retained_push_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let digest = {
        let (_state, _app, _group, digest) = fenced_push(&dir).await;
        digest
    };

    let (state, app) = hosted_writable_distributed(&dir, TOKEN);
    bind_ownership(&state, EpochAuthority::settled(6));
    let finalized = finalize(&state).await;

    assert_eq!((finalized, pull_blob(&app, &digest).await), (1, StatusCode::OK));
}

#[tokio::test]
async fn test_a_published_push_settles_its_intent_in_the_request_that_made_it() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable_distributed(&dir, TOKEN);
    let digest = oci_digest(LAYER);
    assert_eq!(push_blob(&app, &digest).await, StatusCode::CREATED);

    assert_eq!(retained(&state, &digest), Some(IntentPhase::Admitted));
}

#[tokio::test]
async fn test_a_settled_push_is_not_republished_by_a_later_sweep() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable_distributed(&dir, TOKEN);
    assert_eq!(push_blob(&app, &oci_digest(LAYER)).await, StatusCode::CREATED);

    assert_eq!(finalize(&state).await, 0);
}

#[tokio::test]
async fn test_a_push_whose_digest_disagrees_with_its_bytes_retains_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable_distributed(&dir, TOKEN);
    let wrong = oci_digest(b"a-different-layer");

    let status = push_blob(&app, &wrong).await;

    assert_eq!(
        (status, state.serving.meta.count_staged_intents().unwrap()),
        (StatusCode::BAD_REQUEST, 0)
    );
}

#[tokio::test]
async fn test_a_fenced_resumable_push_keeps_the_session_its_home_closes() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable_distributed(&dir, TOKEN);
    let group = EpochAuthority::superseded(5, 6);
    bind_ownership(&state, group.clone());
    let digest = oci_digest(LAYER);
    let (_, headers, _) = send_body(
        &app,
        Method::POST,
        "/v2/store/app/blobs/uploads/",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    let location = headers[header::LOCATION].to_str().unwrap().to_owned();
    send_body(
        &app,
        Method::PATCH,
        &location,
        &[("authorization", &auth(TOKEN))],
        LAYER.to_vec(),
    )
    .await;
    let (status, ..) = send_body(
        &app,
        Method::PUT,
        &format!("{location}?digest={digest}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    group.settle();
    let finalized = finalize(&state).await;

    assert_eq!(
        (
            finalized,
            pull_blob(&app, &digest).await,
            send_with(&app, Method::GET, &location, &[("authorization", &auth(TOKEN))])
                .await
                .0
        ),
        (1, StatusCode::OK, StatusCode::NOT_FOUND)
    );
}

/// Retain an intent under the key this push will mint, bound to different content, so admission has to
/// shed rather than resolve the push onto a record that names another upload.
fn occupy_intent_key(state: &AppState, digest: &str) {
    state
        .serving
        .meta
        .stage_intent(
            IntentAdmission {
                authority: "oci:app",
                key: &intent_key(digest),
                digest: &oci_digest(b"a-different-layer"),
                size: 1,
                payload: b"{}",
            },
            IntentLimits {
                max_records: 8,
                max_bytes: 1 << 20,
                backpressure_percent: 80,
            },
            10,
        )
        .unwrap();
}

#[tokio::test]
async fn test_a_push_admission_cannot_retain_is_shed_unpublished() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable_distributed(&dir, TOKEN);
    let digest = oci_digest(LAYER);
    occupy_intent_key(&state, &digest);

    let status = push_blob(&app, &digest).await;

    assert_eq!(
        (status, pull_blob(&app, &digest).await),
        (StatusCode::SERVICE_UNAVAILABLE, StatusCode::NOT_FOUND)
    );
}

#[tokio::test]
async fn test_a_resumable_push_admission_cannot_retain_is_shed_unpublished() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable_distributed(&dir, TOKEN);
    let digest = oci_digest(LAYER);
    occupy_intent_key(&state, &digest);
    let (_, headers, _) = send_body(
        &app,
        Method::POST,
        "/v2/store/app/blobs/uploads/",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    let location = headers[header::LOCATION].to_str().unwrap().to_owned();
    send_body(
        &app,
        Method::PATCH,
        &location,
        &[("authorization", &auth(TOKEN))],
        LAYER.to_vec(),
    )
    .await;

    let (status, ..) = send_body(
        &app,
        Method::PUT,
        &format!("{location}?digest={digest}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;

    assert_eq!(
        (status, pull_blob(&app, &digest).await),
        (StatusCode::SERVICE_UNAVAILABLE, StatusCode::NOT_FOUND)
    );
}

/// The authority an operator drain names is the canonical OCI repository key, and it is the one the
/// staging record carries, so a drain of that authority offers the write a fenced push retained.
#[tokio::test]
async fn test_a_fenced_push_is_listed_under_the_authority_a_drain_names() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _app, _group, digest) = fenced_push(&dir).await;

    let pending = state
        .serving
        .meta
        .list_pending_intents_for(&crate::name::authority_key("app"), None, 10)
        .unwrap();

    assert_eq!(
        pending.into_iter().map(|(key, _)| key).collect::<Vec<_>>(),
        vec![intent_key(&digest)]
    );
}

#[tokio::test]
async fn test_a_drain_of_another_repository_does_not_offer_this_write() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _app, _group, _digest) = fenced_push(&dir).await;

    let pending = state
        .serving
        .meta
        .list_pending_intents_for(&crate::name::authority_key("other"), None, 10)
        .unwrap();

    assert_eq!(pending, Vec::new());
}

#[tokio::test]
async fn test_a_drain_through_the_registered_capability_publishes_the_retained_write() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app, group, digest) = fenced_push(&dir).await;
    group.settle();

    let settled = drain_one(&state, &crate::name::authority_key("app"), &intent_key(&digest)).await;

    assert_eq!((settled, pull_blob(&app, &digest).await), (true, StatusCode::OK));
}

#[tokio::test]
async fn test_a_drain_through_the_registered_capability_declines_another_authority() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app, group, digest) = fenced_push(&dir).await;
    group.settle();

    let settled = drain_one(&state, &crate::name::authority_key("other"), &intent_key(&digest)).await;

    assert_eq!(
        (settled, pull_blob(&app, &digest).await),
        (false, StatusCode::NOT_FOUND)
    );
}

/// An app over a store whose backend `fault` can be armed mid-request, so a test can let the ingress
/// intent commit and then fail the metadata transaction that publishes the membership.
fn faulted_app(dir: &tempfile::TempDir, meta: peryx_storage::meta::MetaStore) -> (Arc<AppState>, axum::Router) {
    let mut state = AppState::with_clock(
        meta,
        peryx_storage::blob::BlobStore::new(dir.path().join("blobs")),
        60,
        vec![super::writable_index("store", "store", true, TOKEN)],
        Arc::new(|| 1000),
    );
    super::install_oci(&mut state, std::collections::HashMap::new(), false);
    super::install_test_distributed(&mut state, None, Arc::new(super::LocalDurability));
    let state = Arc::new(state);
    (state.clone(), peryx_http::router(state))
}

/// Park a push inside `begin_epoch_write`, which its ingress intent has already committed by, then fail
/// every later store operation so the membership transaction is the one that faults.
async fn fault_the_membership_commit(
    fault: &Arc<peryx_test_support::fault::Fault>,
    entered: &tokio::sync::Semaphore,
    released: &tokio::sync::Semaphore,
    push: tokio::task::JoinHandle<StatusCode>,
) -> StatusCode {
    entered
        .acquire()
        .await
        .expect("the push reaches the epoch write")
        .forget();
    fault.arm(0);
    released.add_permits(1);
    push.await.expect("the push finishes")
}

#[tokio::test]
async fn test_a_monolithic_push_reports_a_membership_store_fault() {
    let dir = tempfile::tempdir().unwrap();
    let (pages, fault) = peryx_test_support::fault::backend();
    let meta =
        peryx_storage::meta::MetaStore::open_backend(peryx_test_support::fault::faulted(&pages, &fault)).unwrap();
    let (state, app) = faulted_app(&dir, meta);
    let (group, entered, released) = EpochAuthority::gated(4);
    bind_ownership(&state, group);
    let digest = oci_digest(LAYER);
    let pushed = digest.clone();
    let pushing = app.clone();
    let push = tokio::spawn(async move { push_blob(&pushing, &pushed).await });

    let status = fault_the_membership_commit(&fault, &entered, &released, push).await;

    assert_eq!((status, fault.triggered()), (StatusCode::BAD_GATEWAY, true));
}

#[tokio::test]
async fn test_a_resumable_push_reports_a_membership_store_fault() {
    let dir = tempfile::tempdir().unwrap();
    let (pages, fault) = peryx_test_support::fault::backend();
    let meta =
        peryx_storage::meta::MetaStore::open_backend(peryx_test_support::fault::faulted(&pages, &fault)).unwrap();
    let (state, app) = faulted_app(&dir, meta);
    let (group, entered, released) = EpochAuthority::gated(4);
    bind_ownership(&state, group);
    let digest = oci_digest(LAYER);
    let (_, headers, _) = send_body(
        &app,
        Method::POST,
        "/v2/store/app/blobs/uploads/",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    let location = headers[header::LOCATION].to_str().unwrap().to_owned();
    send_body(
        &app,
        Method::PATCH,
        &location,
        &[("authorization", &auth(TOKEN))],
        LAYER.to_vec(),
    )
    .await;
    let finishing = app.clone();
    let push = tokio::spawn(async move {
        send_body(
            &finishing,
            Method::PUT,
            &format!("{location}?digest={digest}"),
            &[("authorization", &auth(TOKEN))],
            Vec::new(),
        )
        .await
        .0
    });

    let status = fault_the_membership_commit(&fault, &entered, &released, push).await;

    assert_eq!((status, fault.triggered()), (StatusCode::BAD_GATEWAY, true));
}

fn tag_row(meta: &peryx_storage::meta::MetaStore) -> Option<Vec<u8>> {
    meta.get_driver_value("oci\u{0}t\u{0}store\u{0}app\u{0}latest").unwrap()
}

fn seed_tagged_manifest(meta: &peryx_storage::meta::MetaStore) -> String {
    let manifest = crate::store::Manifest {
        media_type: "application/vnd.oci.image.manifest.v1+json".to_owned(),
        bytes: b"{}".to_vec(),
    };
    let digest = format!("sha256:{}", peryx_storage::blob::Digest::of(&manifest.bytes).as_str());
    crate::store::record_manifest(meta, "store", "app", &digest, &manifest).unwrap();
    crate::store::put_tag(meta, "store", "app", "latest", &digest).unwrap();
    digest
}

/// A tag delete that cannot finish must leave the tag either exactly as it was or gone, never
/// pointing somewhere else. The delete is a soft one, so a failure part way through can legitimately
/// have trashed the tag already; what it may not do is leave a half-written mapping behind for a
/// pull to resolve.
///
/// Each step builds its own store, since a delete that succeeds changes what the next one would see.
#[tokio::test]
async fn test_manifest_tag_delete_never_leaves_a_torn_mapping() {
    let mut failed = 0_u32;
    for fail_after in 0..160 {
        let dir = tempfile::tempdir().unwrap();
        let (pages, fault) = peryx_test_support::fault::backend();
        let meta =
            peryx_storage::meta::MetaStore::open_backend(peryx_test_support::fault::faulted(&pages, &fault)).unwrap();
        seed_tagged_manifest(&meta);
        let intact = tag_row(&meta).expect("the seed wrote a tag");
        let (state, app) = faulted_app(&dir, meta);
        fault.arm(fail_after);
        let status = send_body(
            &app,
            Method::DELETE,
            "/v2/store/app/manifests/latest",
            &[("authorization", &auth(TOKEN))],
            Vec::new(),
        )
        .await
        .0;
        fault.disable();
        drop(app);
        drop(state);

        let meta =
            peryx_storage::meta::MetaStore::reopen_backend(peryx_test_support::fault::faulted(&pages, &fault)).unwrap();
        failed += u32::from(!status.is_success());
        // A trashed tag is gone from this row, which is the delete doing its job. The row that must
        // never appear is a third value: a mapping neither the seed nor the delete wrote.
        if let Some(row) = tag_row(&meta) {
            assert_eq!(
                row, intact,
                "injecting after {fail_after} reads left a rewritten tag while answering {status}"
            );
        }
    }

    assert!(failed > 0, "no injection point reached the delete");
}

/// The same guarantee for the other direction. A restore that cannot finish must leave the tag
/// either still trashed or pointing back at exactly what it pointed at before, never at a digest
/// neither the original write nor the restore put there.
///
/// The delete that sets this up runs unfaulted, so the injection lands on the restore rather than on
/// the trashing that precedes it.
#[tokio::test]
async fn test_manifest_tag_restore_never_leaves_a_torn_mapping() {
    let mut failed = 0_u32;
    for fail_after in 0..160 {
        let dir = tempfile::tempdir().unwrap();
        let (pages, fault) = peryx_test_support::fault::backend();
        let meta =
            peryx_storage::meta::MetaStore::open_backend(peryx_test_support::fault::faulted(&pages, &fault)).unwrap();
        seed_tagged_manifest(&meta);
        let intact = tag_row(&meta).expect("the seed wrote a tag");
        let (state, app) = faulted_app(&dir, meta);
        send_body(
            &app,
            Method::DELETE,
            "/v2/store/app/manifests/latest",
            &[("authorization", &auth(TOKEN))],
            Vec::new(),
        )
        .await;
        fault.arm(fail_after);
        let status = send_body(
            &app,
            Method::PUT,
            "/v2/store/app/manifests/latest/restore",
            &[("authorization", &auth(TOKEN))],
            Vec::new(),
        )
        .await
        .0;
        fault.disable();
        drop(app);
        drop(state);

        let meta =
            peryx_storage::meta::MetaStore::reopen_backend(peryx_test_support::fault::faulted(&pages, &fault)).unwrap();
        failed += u32::from(!status.is_success());
        if let Some(row) = tag_row(&meta) {
            assert_eq!(
                row, intact,
                "injecting after {fail_after} reads restored a tag to a digest nobody wrote, answering {status}"
            );
        }
    }

    assert!(failed > 0, "no injection point reached the restore");
}
