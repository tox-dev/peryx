//! The gap bound the server edge puts on a request body, seen through the handler that streams one.
//! A chunk that stops arriving ends; a chunk that keeps arriving does not, however long it takes.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::task::Poll;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::{Method, Request, StatusCode, header};
use futures_util::{StreamExt as _, stream};
use http_body_util::BodyExt as _;
use peryx_driver::AppState;
use tokio::sync::oneshot;
use tower::ServiceExt as _;

use super::{auth, send_body};
use crate::upload_session::UploadStore as _;

const TOKEN: &str = "s3cret";
/// The gap the server edge allows a request body, named here because the reclaim measurement below is
/// about that number: a change to it changes how long one stalled client delays a pass.
const STALL_BOUND: Duration = Duration::from_secs(30);
/// Under the bound, so a body that pauses this long between frames is still making progress.
const SHORT_OF_THE_BOUND: Duration = Duration::from_secs(25);

fn registry(dir: &tempfile::TempDir) -> (Arc<AppState>, axum::Router) {
    let clock = Arc::new(AtomicI64::new(1000));
    crate::tests::hosted_with_clock(dir, TOKEN, Arc::new(move || clock.load(Ordering::Relaxed)))
}

/// Open a session holding `chunk`, handing back its id and the offset it stands at.
async fn open_session(app: &axum::Router, chunk: &[u8]) -> (String, u64) {
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
    (location.rsplit('/').next().unwrap().to_owned(), chunk.len() as u64)
}

async fn patch_stream(app: axum::Router, session: &str, body: Body) -> StatusCode {
    let request = Request::builder()
        .method(Method::PATCH)
        .uri(format!("/v2/store/app/blobs/uploads/{session}"))
        .header("authorization", auth(TOKEN))
        .body(body)
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    response.into_body().collect().await.unwrap();
    status
}

/// A client that stops sending mid-chunk gives up its handler, and the session it was appending to
/// stands at the bytes that reached disk. Time is virtual, so the bound fires the moment every task is
/// idle, which is the state that says the body has stopped arriving.
#[tokio::test(start_paused = true)]
async fn test_a_stalled_chunk_leaves_a_session_the_client_can_resume() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = registry(&dir);
    let (session, opened) = open_session(&app, b"first").await;
    // The second frame never arrives, which is the whole of what a stalled client does.
    let stalling = stream::once(async { Ok::<_, std::io::Error>(Bytes::from_static(b"second")) })
        .chain(stream::once(std::future::pending()));

    let stalled = patch_stream(app.clone(), &session, Body::from_stream(stalling)).await;
    let landed = state.serving.meta.upload_record(&session).unwrap();
    let resumed = patch_stream(app.clone(), &session, Body::from("third")).await;

    assert_eq!(
        stalled,
        StatusCode::REQUEST_TIMEOUT,
        "a client that stopped sending is told the server gave up waiting, not that an upstream failed"
    );
    assert_eq!(
        landed.map(|record| record.offset),
        Some(opened + 6),
        "the session stands at the bytes that reached disk",
    );
    assert_eq!(
        resumed,
        StatusCode::ACCEPTED,
        "the client resumes the session it still has"
    );
    assert_eq!(
        state.serving.meta.upload_record(&session).unwrap().map(|r| r.offset),
        Some(opened + 11),
    );
}

/// The bound is on the gap, so a chunk that keeps arriving is never cut however long it takes in total.
/// Three frames spaced just under the bound run well past it and still land.
#[tokio::test(start_paused = true)]
async fn test_a_slow_chunk_that_keeps_arriving_is_not_cut() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = registry(&dir);
    let (session, opened) = open_session(&app, b"first").await;
    let trickle = stream::iter([
        Bytes::from_static(b"aa"),
        Bytes::from_static(b"bb"),
        Bytes::from_static(b"cc"),
    ])
    .then(|frame| async move {
        tokio::time::sleep(SHORT_OF_THE_BOUND).await;
        Ok::<_, std::io::Error>(frame)
    });

    let started = tokio::time::Instant::now();
    let status = patch_stream(app.clone(), &session, Body::from_stream(trickle)).await;
    let elapsed = started.elapsed();

    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(
        state.serving.meta.upload_record(&session).unwrap().map(|r| r.offset),
        Some(opened + 6),
    );
    assert!(
        elapsed > STALL_BOUND * 2,
        "the transfer outlived the bound it never tripped"
    );
}

/// Far enough past the session TTL that every open session is a reclamation candidate.
const LONG_AFTER: i64 = 1_000_000;

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

/// What the bound does for the reclaim pass, which is the case this bound exists for. The pass takes
/// each expired session's gate in turn, so a stalled `PATCH` holding one used to delay every session
/// behind it for as long as the client stayed connected. The delay is now one stall bound, after which
/// the pass reclaims the stalled session and the one behind it.
#[tokio::test(start_paused = true)]
async fn test_a_stalled_chunk_delays_the_reclaim_pass_by_one_bound() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(AtomicI64::new(1000));
    let ticking = clock.clone();
    let (state, app) = crate::tests::hosted_with_clock(&dir, TOKEN, Arc::new(move || ticking.load(Ordering::Relaxed)));
    let (parked, holding) = oneshot::channel();
    let (stalled_session, _) = open_session(&app, b"first").await;
    let (other_session, _) = open_session(&app, b"other").await;
    clock.store(LONG_AFTER, Ordering::Relaxed);
    // The body sends nothing at all, so it holds the gate without refreshing the row the pass selected.
    // Its first poll is the point the handler has the gate, which is what the signal reports.
    let mut arrival = Some(parked);
    let stalling = stream::poll_fn(move |_| {
        if let Some(arrival) = arrival.take() {
            arrival.send(()).unwrap();
        }
        Poll::<Option<Result<Bytes, std::io::Error>>>::Pending
    });
    let appending = tokio::spawn({
        let (app, session) = (app.clone(), stalled_session.clone());
        async move { patch_stream(app, &session, Body::from_stream(stalling)).await }
    });
    holding.await.unwrap();

    let started = tokio::time::Instant::now();
    let reclaimed = reclaim(&state).await;
    let waited = started.elapsed();

    assert_eq!(appending.await.unwrap(), StatusCode::REQUEST_TIMEOUT);
    assert_eq!(reclaimed, 2, "the stalled session and the one queued behind it both go");
    assert!(
        (STALL_BOUND..STALL_BOUND * 2).contains(&waited),
        "the pass waits one stall bound, not for as long as the client stays connected",
    );
    assert_eq!(state.serving.meta.upload_record(&other_session).unwrap(), None);
}
