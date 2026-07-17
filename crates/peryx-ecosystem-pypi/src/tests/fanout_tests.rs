//! Concurrent cold downloads: one upstream transfer feeds every waiting client as bytes arrive.

use std::sync::Arc;

use bytes::Bytes;
use futures_util::StreamExt as _;
use peryx_storage::blob::{BlobTail, Digest};
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use super::http::{detail_json, get, harness};
use crate::cache;
use crate::store::PypiStore as _;
use peryx_driver::download::{DownloadHandle, DownloadProgress};
use peryx_driver::state::AppState;

/// A stalling upstream: sends the header and the first half of the body, waits for the release
/// signal, then sends the rest. Accepts exactly one connection, so a second upstream fetch for
/// the same file hangs the test instead of passing silently.
fn stalling_upstream(first: Vec<u8>, rest: Vec<u8>) -> (String, std::sync::mpsc::Sender<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (release, released) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        use std::io::{Read as _, Write as _};
        let (mut socket, _) = listener.accept().unwrap();
        let mut buffer = [0u8; 1024];
        let _ = socket.read(&mut buffer);
        let header = format!(
            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n",
            first.len() + rest.len()
        );
        socket.write_all(header.as_bytes()).unwrap();
        socket.write_all(&first).unwrap();
        socket.flush().unwrap();
        released.recv().unwrap();
        socket.write_all(&rest).unwrap();
    });
    (format!("http://{addr}/stalled.whl"), release)
}

fn truncated_upstream() -> (String, std::sync::mpsc::Sender<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (release, released) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        use std::io::{Read as _, Write as _};
        let (mut socket, _) = listener.accept().unwrap();
        let mut buffer = [0u8; 1024];
        let _ = socket.read(&mut buffer);
        socket
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\npart")
            .unwrap();
        socket.flush().unwrap();
        released.recv().unwrap();
    });
    (format!("http://{addr}/truncated.whl"), release)
}

async fn live_stream_for(state: &Arc<AppState>, digest: &Digest) -> cache::FileOutcome {
    cache::stream_file(
        state.serving.clone(),
        digest.clone(),
        "pypi".to_owned(),
        "stalled.whl".to_owned(),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn test_concurrent_cold_requests_stream_before_the_transfer_finishes() {
    let h = harness().await;
    // Both halves clear the flush threshold so tails see bytes while the upstream stalls.
    let first = vec![0xAAu8; 400 * 1024];
    let rest = vec![0xBBu8; 300 * 1024];
    let mut whole = first.clone();
    whole.extend_from_slice(&rest);
    let digest = Digest::of(&whole);
    let (url, release) = stalling_upstream(first.clone(), rest);
    h.state.meta.put_file_url(digest.as_str(), &url, "pypi").unwrap();

    let cache::FileOutcome::Live(mut leader) = live_stream_for(&h.state, &digest).await else {
        panic!("expected a live stream");
    };
    let cache::FileOutcome::Live(mut follower) = live_stream_for(&h.state, &digest).await else {
        panic!("expected the follower to attach to the live transfer");
    };
    // Both clients receive bytes while the upstream is still stalled: parallel feeding, not
    // wait-for-commit.
    let leader_first = leader.next().await.unwrap().unwrap();
    let follower_first = follower.next().await.unwrap().unwrap();
    assert!(!leader_first.is_empty());
    assert!(!follower_first.is_empty());

    release.send(()).unwrap();
    for (stream, mut body) in [
        (&mut leader, leader_first.to_vec()),
        (&mut follower, follower_first.to_vec()),
    ] {
        while let Some(chunk) = stream.next().await {
            body.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(body, whole);
    }
    assert!(h.state.blobs.head(&digest).await.unwrap().is_some());
}

#[tokio::test]
async fn test_client_arriving_after_commit_streams_from_disk() {
    let h = harness().await;
    let body = vec![0xCCu8; 8 * 1024];
    let digest = Digest::of(&body);
    let (url, release) = stalling_upstream(body[..4096].to_vec(), body[4096..].to_vec());
    h.state.meta.put_file_url(digest.as_str(), &url, "pypi").unwrap();
    let cache::FileOutcome::Live(mut leader) = live_stream_for(&h.state, &digest).await else {
        panic!("expected a live stream");
    };
    release.send(()).unwrap();
    let mut streamed = Vec::new();
    while let Some(chunk) = leader.next().await {
        streamed.extend_from_slice(&chunk.unwrap());
    }
    assert_eq!(streamed, body);
    let outcome = live_stream_for(&h.state, &digest).await;
    assert!(matches!(outcome, cache::FileOutcome::Cached(_)));
}

#[tokio::test]
async fn test_blob_committed_while_waiting_on_the_gate_serves_from_disk() {
    let h = harness().await;
    let body = b"landed while parked";
    let digest = Digest::of(body);
    let gate = cache::flight_gate(&h.state, digest.as_str());
    let guard = gate.lock_owned().await;
    let waiting = tokio::spawn({
        let state = h.state.serving.clone();
        let digest = digest.clone();
        async move { cache::stream_file(state, digest, "pypi".to_owned(), "parked.whl".to_owned()).await }
    });
    // The parked request only proceeds once the holder commits the blob and releases the gate.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    h.state.blobs.put_bytes_as(body, &digest).await.unwrap();
    drop(guard);
    let outcome = waiting.await.unwrap().unwrap();
    assert!(matches!(outcome, cache::FileOutcome::Cached(_)));
}

#[tokio::test]
async fn test_digest_mismatch_fails_every_tail_and_persists_nothing() {
    let h = harness().await;
    let digest = Digest::of(b"what the page promised");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            detail_json(digest.as_str(), &file_url).into_bytes(),
            "application/vnd.pypi.simple.v1+json",
        ))
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/flask.whl"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"what upstream delivered".to_vec()))
        .mount(&h.server)
        .await;
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    let outcomes =
        futures_util::future::join_all([live_stream_for(&h.state, &digest), live_stream_for(&h.state, &digest)]).await;
    for outcome in outcomes {
        let cache::FileOutcome::Live(mut stream) = outcome else {
            panic!("expected a live stream");
        };
        let mut saw_error = false;
        while let Some(item) = stream.next().await {
            saw_error |= item.is_err();
        }
        assert!(saw_error);
    }
    assert!(h.state.blobs.head(&digest).await.unwrap().is_none());
}

#[tokio::test]
async fn test_abandoned_download_still_fills_the_cache() {
    let h = harness().await;
    let body = vec![0xDDu8; 16 * 1024];
    let digest = Digest::of(&body);
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    Mock::given(method("GET"))
        .and(path("/files/flask.whl"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .mount(&h.server)
        .await;
    h.state.meta.put_file_url(digest.as_str(), &file_url, "pypi").unwrap();
    let outcome = live_stream_for(&h.state, &digest).await;
    assert!(matches!(outcome, cache::FileOutcome::Live(_)));
    drop(outcome);
    for _ in 0..200 {
        if h.state.blobs.head(&digest).await.unwrap().is_some() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("the detached transfer never persisted the blob");
}

#[tokio::test]
async fn test_stage_cleanup_error_removes_the_live_download() {
    let h = harness().await;
    let digest = Digest::of(b"complete body");
    let (url, release) = truncated_upstream();
    h.state.meta.put_file_url(digest.as_str(), &url, "pypi").unwrap();
    let outcome = live_stream_for(&h.state, &digest).await;
    let stage = std::fs::read_dir(h.dir.path().join("blobs"))
        .unwrap()
        .find(|entry| entry.as_ref().is_ok_and(|entry| entry.file_type().unwrap().is_file()))
        .unwrap()
        .unwrap()
        .path();
    std::fs::remove_file(&stage).unwrap();
    std::fs::create_dir(&stage).unwrap();
    release.send(()).unwrap();
    drop(outcome);

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if !h
                .state
                .downloads
                .lock()
                .expect("downloads lock")
                .contains_key(digest.as_str())
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(h.state.blobs.head(&digest).await.unwrap().is_none());
}

fn handle_with(tail: BlobTail, progress: DownloadProgress) -> DownloadHandle {
    let (sender, receiver) = tokio::sync::watch::channel(progress);
    // The sender leaks into a detached task keeping the channel open for the test body.
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_mins(1)).await;
        drop(sender);
    });
    DownloadHandle::new(tail, receiver)
}

async fn missing_tail(state: &AppState) -> BlobTail {
    let pending = state.blobs.begin().await.unwrap();
    let tail = pending.tail().unwrap();
    pending.abort().await.unwrap();
    tail
}

async fn drain(state: &Arc<AppState>, digest: Digest, handle: DownloadHandle) -> Result<Vec<u8>, std::io::Error> {
    let mut stream = cache::tail_download(
        state.serving.clone(),
        digest,
        handle,
        "pypi".to_owned(),
        "tail.whl".to_owned(),
    );
    let mut body = Vec::new();
    while let Some(item) = stream.next().await {
        body.extend_from_slice(&item?);
    }
    Ok(body)
}

#[tokio::test]
async fn test_tail_of_a_truncated_temp_file_errors() {
    let h = harness().await;
    let mut pending = h.state.blobs.begin().await.unwrap();
    pending.write_chunk(Bytes::from_static(b"abc")).await.unwrap();
    pending.flush().await.unwrap();
    let progress = DownloadProgress {
        flushed: 100,
        done: None,
    };
    let handle = handle_with(pending.tail().unwrap(), progress);
    // Three bytes arrive, then the read inside the flushed window comes back empty.
    let mut stream = cache::tail_download(
        h.state.serving.clone(),
        Digest::of(b"tail-target"),
        handle,
        "pypi".to_owned(),
        "tail.whl".to_owned(),
    );
    assert_eq!(stream.next().await.unwrap().unwrap(), Bytes::from_static(b"abc"));
    let err = stream.next().await.unwrap().unwrap_err();
    assert!(err.to_string().contains("truncated"));
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn test_tail_switches_to_the_committed_blob_when_the_temp_file_is_gone() {
    let h = harness().await;
    let body = b"committed while attaching";
    let digest = Digest::of(body);
    h.state.blobs.put_bytes_as(body, &digest).await.unwrap();
    let progress = DownloadProgress {
        flushed: body.len() as u64,
        done: Some(Ok(())),
    };
    let handle = handle_with(missing_tail(&h.state).await, progress);
    let mut stream = cache::tail_download(
        h.state.serving.clone(),
        digest,
        handle,
        "pypi".to_owned(),
        "tail.whl".to_owned(),
    );
    let mut streamed = Vec::new();
    while let Some(item) = stream.next().await {
        streamed.extend_from_slice(&item.unwrap());
    }
    assert_eq!(streamed, body);
}

#[tokio::test]
async fn test_committed_tail_holds_its_materialized_lease_until_eof() {
    let h = harness().await;
    let body = b"committed while attaching";
    let digest = Digest::of(body);
    h.state.blobs.put_bytes_as(body, &digest).await.unwrap();
    let progress = DownloadProgress {
        flushed: body.len() as u64,
        done: Some(Ok(())),
    };
    let handle = handle_with(missing_tail(&h.state).await, progress);
    let mut stream = cache::tail_download(
        h.state.serving.clone(),
        digest,
        handle,
        "pypi".to_owned(),
        "tail.whl".to_owned(),
    );
    assert_eq!(stream.next().await.unwrap().unwrap(), body.as_slice());
    assert_eq!(
        std::fs::read_dir(h.dir.path().join("blobs/.leases"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".peryx-lease-"))
            .count(),
        1
    );
    assert!(stream.next().await.is_none());
    assert!(
        std::fs::read_dir(h.dir.path().join("blobs/.leases"))
            .unwrap()
            .filter_map(Result::ok)
            .all(|entry| !entry.file_name().to_string_lossy().starts_with(".peryx-lease-"))
    );
}

#[tokio::test]
async fn test_backend_without_local_tail_records_the_committed_download() {
    let h = harness().await;
    let body = b"streamed from committed storage";
    let digest = h.state.blobs.put_bytes(body).await.unwrap();
    let (sender, receiver) = tokio::sync::watch::channel(DownloadProgress {
        flushed: body.len() as u64,
        done: Some(Ok(())),
    });
    let streamed = drain(&h.state, digest, DownloadHandle::new(None, receiver))
        .await
        .unwrap();
    drop(sender);
    assert_eq!(streamed, body);
    for _ in 0..500 {
        if h.state
            .metrics
            .index_totals()
            .get("pypi")
            .is_some_and(|totals| totals.base.downloads == 1)
        {
            assert_eq!(h.state.metrics.index_totals()["pypi"].base.bytes, body.len() as u64);
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    panic!("download metric never settled");
}

#[tokio::test]
async fn test_tail_waits_out_the_commit_window_between_rename_and_verdict() {
    let h = harness().await;
    let body = b"renamed before the verdict broadcast";
    let digest = Digest::of(body);
    let progress = DownloadProgress {
        flushed: body.len() as u64,
        done: None,
    };
    // The temp file is already gone but no verdict has landed: the exact in-between state a slow
    // tail observes mid-commit. The verdict arrives while it waits.
    let (sender, receiver) = tokio::sync::watch::channel(progress);
    let handle = DownloadHandle::new(missing_tail(&h.state).await, receiver);
    let state = h.state.clone();
    let commit_digest = digest.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        state.blobs.put_bytes_as(body, &commit_digest).await.unwrap();
        sender.send_modify(|progress| progress.done = Some(Ok(())));
    });
    let streamed = drain(&h.state, digest, handle).await.unwrap();
    assert_eq!(streamed, body);
}

#[tokio::test]
async fn test_tail_with_a_missing_temp_file_surfaces_the_failure_verdict() {
    let h = harness().await;
    let progress = DownloadProgress {
        flushed: 10,
        done: Some(Err("verification failed".to_owned())),
    };
    let handle = handle_with(missing_tail(&h.state).await, progress);
    let err = drain(&h.state, Digest::of(b"tail-target"), handle).await.unwrap_err();
    assert!(err.to_string().contains("verification failed"));
}

#[tokio::test]
async fn test_tail_with_a_missing_temp_file_and_a_dead_pump_errors() {
    let h = harness().await;
    let progress = DownloadProgress {
        flushed: 10,
        done: None,
    };
    let (sender, receiver) = tokio::sync::watch::channel(progress);
    drop(sender);
    let handle = DownloadHandle::new(missing_tail(&h.state).await, receiver);
    let err = drain(&h.state, Digest::of(b"tail-target"), handle).await.unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}

#[tokio::test]
async fn test_tail_surfaces_the_transfer_failure() {
    let h = harness().await;
    let progress = DownloadProgress {
        flushed: 0,
        done: Some(Err("upstream fell over".to_owned())),
    };
    let handle = handle_with(missing_tail(&h.state).await, progress);
    let err = drain(&h.state, Digest::of(b"tail-target"), handle).await.unwrap_err();
    assert!(err.to_string().contains("upstream fell over"));
}

#[tokio::test]
async fn test_tail_errors_when_the_pump_vanishes_without_a_verdict() {
    let h = harness().await;
    let (sender, receiver) = tokio::sync::watch::channel(DownloadProgress::default());
    drop(sender);
    let handle = DownloadHandle::new(missing_tail(&h.state).await, receiver);
    let err = drain(&h.state, Digest::of(b"tail-target"), handle).await.unwrap_err();
    assert!(err.to_string().contains("abandoned"));
}
