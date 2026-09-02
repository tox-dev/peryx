//! Reclaiming an idle upload: what survives a backend that will not delete the stage, and what a
//! request appending to that session sees while maintenance is looking at it.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use axum::body::{Body, Bytes};
use axum::http::{Method, Request, StatusCode, header};
use futures_util::{StreamExt as _, stream};
use http_body_util::BodyExt as _;
use peryx_driver::AppState;
use peryx_storage::meta::MetaStore;
use tokio::sync::oneshot;
use tower::ServiceExt as _;

use super::{auth, body_has_code, send_body};
use crate::upload_session::UploadStore as _;

const TOKEN: &str = "s3cret";
/// Far enough past the session TTL that every open session is a reclamation candidate.
const LONG_AFTER: i64 = 1_000_000;

/// A hosted registry whose clock the caller advances to age a session past its idle window.
fn registry(dir: &tempfile::TempDir) -> (Arc<AppState>, axum::Router, Arc<AtomicI64>) {
    let now = Arc::new(AtomicI64::new(1000));
    let ticking = now.clone();
    let (state, app) = crate::tests::hosted_with_clock(dir, TOKEN, Arc::new(move || ticking.load(Ordering::Relaxed)));
    (state, app, now)
}

/// Open a session and stage one chunk into it, handing back the session id.
async fn open_session(app: &axum::Router, chunk: &[u8]) -> String {
    let (status, headers, _) = send_body(
        app,
        Method::POST,
        "/v2/store/app/blobs/uploads/",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let location = headers[header::LOCATION].to_str().unwrap().to_owned();
    let (status, _, _) = send_body(
        app,
        Method::PATCH,
        &location,
        &[("authorization", &auth(TOKEN))],
        chunk.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    location.rsplit('/').next().unwrap().to_owned()
}

/// `PATCH` one session with a body the caller controls chunk by chunk.
async fn patch_stream(app: axum::Router, session: String, body: Body) -> (StatusCode, Bytes) {
    let request = Request::builder()
        .method(Method::PATCH)
        .uri(format!("/v2/store/app/blobs/uploads/{session}"))
        .header("authorization", auth(TOKEN))
        .body(body)
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    (status, response.into_body().collect().await.unwrap().to_bytes())
}

/// Run one maintenance pass through the driver the router installed, so it contends for the same
/// per-session gate a request holds.
async fn reclaim(state: &Arc<AppState>) -> usize {
    let reclaimer = state
        .idle_reclaimers()
        .next()
        .expect("the OCI driver registers a reclaimer")
        .1
        .clone();
    reclaimer.reclaim_idle(state.serving.clone()).await
}

#[cfg(unix)]
fn set_mode(path: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// A stage the backend refuses to delete keeps its row, so the next pass still knows which bytes the
/// id names. Removing the row first would have left those bytes with nothing to find them by.
#[cfg(unix)]
#[tokio::test]
async fn test_a_stage_the_backend_will_not_delete_keeps_its_session() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app, now) = registry(&dir);
    let session = open_session(&app, b"a-layer").await;
    now.store(LONG_AFTER, Ordering::Relaxed);
    let uploads = dir.path().join("blobs/uploads");

    set_mode(&uploads, 0o555);
    let retained = reclaim(&state).await;
    set_mode(&uploads, 0o755);

    assert_eq!(retained, 0);
    assert!(state.serving.meta.upload_record(&session).unwrap().is_some());
    assert_eq!(reclaim(&state).await, 1);
    assert_eq!(state.serving.meta.upload_record(&session).unwrap(), None);
}

/// A session whose index left the configuration is still reclaimed, and the stage it retains is
/// counted against no repository because there is no longer one to count it against.
#[cfg(unix)]
#[tokio::test]
async fn test_a_retained_stage_for_a_departed_index_is_still_retried() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app, now) = registry(&dir);
    let session = open_session(&app, b"a-layer").await;
    // The index left the configuration while the session was open, leaving the row naming it.
    state.serving.meta.begin_upload(&session, "gone", "app", 1).unwrap();
    now.store(LONG_AFTER, Ordering::Relaxed);
    let uploads = dir.path().join("blobs/uploads");

    set_mode(&uploads, 0o555);
    assert_eq!(reclaim(&state).await, 0);
    set_mode(&uploads, 0o755);

    assert!(state.serving.meta.upload_record(&session).unwrap().is_some());
    assert_eq!(reclaim(&state).await, 1);
}

/// The retained row is durable, so the retry survives the restart that a transient backend failure
/// often outlives.
#[cfg(unix)]
#[tokio::test]
async fn test_a_retained_session_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app, now) = registry(&dir);
    let session = open_session(&app, b"a-layer").await;
    now.store(LONG_AFTER, Ordering::Relaxed);
    let uploads = dir.path().join("blobs/uploads");

    set_mode(&uploads, 0o555);
    assert_eq!(reclaim(&state).await, 0);
    set_mode(&uploads, 0o755);
    drop(app);
    drop(state);
    let reopened = MetaStore::open(dir.path().join("peryx.redb")).unwrap();

    assert!(reopened.upload_record(&session).unwrap().is_some());
}

/// Maintenance takes the gate a `PATCH` holds, so a request appending to a session it selected
/// finishes against a live row and the refreshed timestamp then keeps that row.
#[tokio::test]
async fn test_a_patch_in_flight_keeps_maintenance_off_its_session() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app, now) = registry(&dir);
    let session = open_session(&app, b"first").await;
    now.store(LONG_AFTER, Ordering::Relaxed);
    // The body parks before its first chunk, so the request holds the gate without having refreshed
    // the row that maintenance selected.
    let (parked, holding) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let body = Body::from_stream(stream::once(async move {
        parked.send(()).unwrap();
        released.await.unwrap();
        Ok::<_, std::io::Error>(Bytes::from_static(b"second"))
    }));
    let appending = tokio::spawn(patch_stream(app.clone(), session.clone(), body));
    holding.await.unwrap();

    let maintenance = tokio::spawn({
        let state = state.clone();
        async move { reclaim(&state).await }
    });
    release.send(()).unwrap();
    let (status, _) = appending.await.unwrap();

    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(maintenance.await.unwrap(), 0);
    assert!(state.serving.meta.upload_record(&session).unwrap().is_some());
}

/// A session removed while a chunk is being written cannot be reported as a successful append: the
/// offset would name an upload the registry no longer has.
#[tokio::test]
async fn test_an_append_whose_session_vanishes_answers_unknown_upload() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app, _now) = registry(&dir);
    let session = open_session(&app, b"first").await;
    // The second item is polled only once the first chunk has been staged and recorded, which is the
    // point the session still exists and the append has committed to an offset.
    let (staged, recorded) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let body = Body::from_stream(
        stream::once(async { Ok::<_, std::io::Error>(Bytes::from_static(b"second")) }).chain(stream::once(
            async move {
                staged.send(()).unwrap();
                released.await.unwrap();
                Ok(Bytes::from_static(b"third"))
            },
        )),
    );
    let appending = tokio::spawn(patch_stream(app.clone(), session.clone(), body));
    recorded.await.unwrap();

    state.serving.meta.remove_upload(&session).unwrap();
    release.send(()).unwrap();
    let (status, body) = appending.await.unwrap();

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body_has_code(&body, "BLOB_UPLOAD_UNKNOWN"), "{body:?}");
}
