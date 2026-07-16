//! File-download streaming: cached blobs, single-flight upstream transfers, and live tailing.

use std::path::PathBuf;
use std::sync::Arc;

use crate::project_of_filename;
use crate::store::PypiStore as _;
use bytes::Bytes;
use peryx_driver::download::{DownloadHandle, DownloadProgress};
use peryx_driver::rate_limit::UpstreamPermit;
use peryx_driver::state::ServingState;
use peryx_events::metrics::Event;
use peryx_storage::blob::Digest;

use super::{CacheError, flight_gate, release_flight, source_client, upstream_permit};

/// Resolve a file to a hosted blob path. A cache miss is fetched through the same streaming path as
/// downloads, so the archive inspector never buffers the whole artifact in memory.
///
/// # Errors
/// Returns [`CacheError::FileNotFound`] if the digest has no known source, or another error on a
/// store or upstream failure.
///
/// # Panics
/// Panics only if the downloads registry lock is poisoned by an earlier panic.
#[expect(
    clippy::significant_drop_tightening,
    reason = "the connect gate stays held across start_download so racing inspectors attach instead of double-fetching"
)]
pub async fn file_path(
    state: Arc<ServingState>,
    digest: Digest,
    route: String,
    filename: String,
) -> Result<PathBuf, CacheError> {
    if state.blobs.exists(&digest) {
        return Ok(state.blobs.path_for(&digest));
    }
    let gate = flight_gate(&state, digest.as_str());
    let guard = gate.lock_owned().await;
    if state.blobs.exists(&digest) {
        release_flight(&state, digest.as_str(), guard);
        return Ok(state.blobs.path_for(&digest));
    }
    let mut handle = if let Some(running) = existing_download(&state, &digest) {
        running
    } else {
        let started = start_download(&state, &digest, route, filename).await?;
        state
            .downloads
            .lock()
            .expect("downloads lock")
            .insert(digest.as_str().to_owned(), started.clone());
        started
    };
    release_flight(&state, digest.as_str(), guard);
    wait_for_download(&mut handle).await?;
    Ok(state.blobs.path_for(&digest))
}

/// What a file is, told without reading a byte of it.
pub enum FileProbe {
    /// The blob is on disk, this long, written at this time when the store can say.
    Cached(u64, Option<std::time::SystemTime>),
    /// Fetchable from upstream, of the size the index page that registered it advertised, if any.
    Upstream(Option<u64>),
}

/// Answer what can be known about a file without its bytes: whether it is served at all, and how long
/// it is.
///
/// A `HEAD` is built from this, so it stops where [`stream_file`] would open an upstream connection.
///
/// The size of an uncached file is whatever the index page recorded (PEP 700 `size`), so an index that
/// publishes none leaves it unknown. Asking upstream for it would trade the body fetch this exists to
/// avoid for a smaller one, and a `HEAD` may omit a `Content-Length` it cannot state truthfully.
///
/// # Errors
/// Returns [`CacheError::FileNotFound`] if the digest has no known source, [`CacheError::OfflineMissing`]
/// if the index it came from is offline, or another error on a store failure.
pub fn probe_file(state: &ServingState, digest: &Digest) -> Result<FileProbe, CacheError> {
    // One stat answers "is it cached", sizes it, and dates it, where the streaming path needs the
    // handle too. The date is what a `GET` of the same blob validates on, so a `HEAD` states it too.
    if let Ok(blob) = std::fs::metadata(state.blobs.path_for(digest)) {
        return Ok(FileProbe::Cached(blob.len(), blob.modified().ok()));
    }
    let source = state
        .meta
        .get_file_url(digest.as_str())?
        .ok_or(CacheError::FileNotFound)?;
    if source_client(state, &source.source, source.upstream.as_deref())?.1 {
        return Err(CacheError::OfflineMissing("file"));
    }
    Ok(FileProbe::Upstream(source.size))
}

/// How a file download gets its bytes.
pub enum FileOutcome {
    /// The blob is on disk; stream it from there.
    Cached(std::path::PathBuf),
    /// A live upstream download, streamed to the client while it verifies and persists.
    Live(futures_util::stream::BoxStream<'static, Result<Bytes, std::io::Error>>),
}

/// Serve a file with maximum overlap: a cached blob streams from disk, a miss tails one shared
/// upstream transfer as its bytes land.
///
/// A miss starts a detached transfer that hashes into a temp file; every concurrent request for
/// the digest streams from that file in parallel, and the transfer outlives its clients, so an
/// abandoned download still populates the cache.
///
/// # Errors
/// Returns [`CacheError::FileNotFound`] if the digest has no known source, or another error on a
/// store or upstream failure.
///
/// # Panics
/// Never in practice: only if the downloads registry lock was poisoned by an earlier panic.
#[expect(
    clippy::significant_drop_tightening,
    reason = "the connect gate stays held across start_download so racing clients attach instead of double-fetching"
)]
pub async fn stream_file(
    state: Arc<ServingState>,
    digest: Digest,
    route: String,
    filename: String,
) -> Result<FileOutcome, CacheError> {
    if state.blobs.exists(&digest) {
        return Ok(FileOutcome::Cached(state.blobs.path_for(&digest)));
    }
    // The gate serializes only the connect phase, so an upstream refusal stays a clean HTTP error
    // for every racing client instead of a mid-body abort on a started stream.
    let gate = flight_gate(&state, digest.as_str());
    let guard = gate.lock_owned().await;
    if state.blobs.exists(&digest) {
        release_flight(&state, digest.as_str(), guard);
        return Ok(FileOutcome::Cached(state.blobs.path_for(&digest)));
    }
    let handle = if let Some(running) = existing_download(&state, &digest) {
        running
    } else {
        let started = start_download(&state, &digest, route.clone(), filename.clone()).await?;
        state
            .downloads
            .lock()
            .expect("downloads lock")
            .insert(digest.as_str().to_owned(), started.clone());
        started
    };
    release_flight(&state, digest.as_str(), guard);
    Ok(FileOutcome::Live(tail_download(state, digest, handle, route, filename)))
}

/// Connect upstream and spawn the detached pump feeding a new blob transfer.
async fn start_download(
    state: &Arc<ServingState>,
    digest: &Digest,
    route: String,
    filename: String,
) -> Result<DownloadHandle, CacheError> {
    let source = state
        .meta
        .get_file_url(digest.as_str())?
        .ok_or(CacheError::FileNotFound)?;
    let (client, offline) = source_client(state, &source.source, source.upstream.as_deref())?;
    if offline {
        return Err(CacheError::OfflineMissing("file"));
    }
    let permit = upstream_permit(state, &source.source).await?;
    let body = client.stream_bytes(&source.url).await?;
    let pending = state.blobs.begin()?;
    let (sender, receiver) = tokio::sync::watch::channel(DownloadProgress::default());
    let handle = DownloadHandle::new(pending.path().to_owned(), receiver);
    let pump_state = state.clone();
    let pump_digest = digest.clone();
    tokio::spawn(async move {
        pump_download(
            pump_state,
            pump_digest,
            body,
            pending,
            sender,
            (route, filename),
            permit,
        )
        .await;
    });
    Ok(handle)
}

/// The in-flight download for `digest`, if one is pumping right now.
fn existing_download(state: &ServingState, digest: &Digest) -> Option<DownloadHandle> {
    state
        .downloads
        .lock()
        .expect("downloads lock")
        .get(digest.as_str())
        .cloned()
}

async fn wait_for_download(handle: &mut DownloadHandle) -> Result<(), CacheError> {
    loop {
        let done = handle.progress().borrow_and_update().done.clone();
        match done {
            Some(Ok(())) => return Ok(()),
            Some(Err(message)) => return Err(CacheError::Stream(message)),
            None => {
                if handle.progress().changed().await.is_err() {
                    return Err(CacheError::Stream("blob transfer abandoned".to_owned()));
                }
            }
        }
    }
}

/// Chunks smaller than this batch up before a flush makes them visible to tailing readers.
const FLUSH_EVERY: u64 = 256 * 1024;

/// Pull the upstream body into the pending blob, publishing flushed progress as it lands; on EOF
/// the blob persists only when the digest verifies (clients check their own hashes regardless).
/// Runs detached from any client, so the cache fills even when every requester disconnects.
async fn pump_download(
    state: Arc<ServingState>,
    digest: Digest,
    body: impl futures_util::Stream<Item = Result<Bytes, peryx_upstream::UpstreamError>> + Send + 'static,
    mut pending: peryx_storage::blob::PendingBlob,
    sender: tokio::sync::watch::Sender<DownloadProgress>,
    served_as: (String, String),
    permit: UpstreamPermit,
) {
    let started = std::time::Instant::now();
    let outcome = match drain_to_blob(body, &mut pending, &sender).await {
        Ok(()) => {
            let commit_state = state.clone();
            let commit_digest = digest.clone();
            // The rename waits on an fsync, so it runs off the async workers.
            tokio::task::spawn_blocking(move || commit_state.blobs.commit(pending, &commit_digest))
                .await
                .expect("blob commit task never panics")
                .map_err(|err| err.to_string())
        }
        Err(err) => Err(err),
    };
    drop(permit);
    let bytes = sender.borrow().flushed;
    let elapsed_ms = started.elapsed().as_millis();
    #[rustfmt::skip]
    tracing::debug!(digest = digest.as_str(), bytes, elapsed_ms, "blob transfer ended");
    if outcome.is_err() {
        tracing::warn!(digest = digest.as_str(), "blob persist rejected");
        let (route, filename) = served_as;
        let project = project_of_filename(&filename);
        state.metrics.record(Event::BlobRejected { route, project });
    }
    // Commit lands before the entry disappears and the entry disappears before done broadcasts,
    // so a request arriving at any point sees the blob, the live download, or nothing stale.
    state.downloads.lock().expect("downloads lock").remove(digest.as_str());
    sender.send_modify(|progress| progress.done = Some(outcome));
}

/// Tee the upstream body into the pending blob, publishing progress at every flush. Errors map to
/// strings so the watch channel can carry the verdict to every tailing client.
async fn drain_to_blob(
    body: impl futures_util::Stream<Item = Result<Bytes, peryx_upstream::UpstreamError>> + Send + 'static,
    pending: &mut peryx_storage::blob::PendingBlob,
    sender: &tokio::sync::watch::Sender<DownloadProgress>,
) -> Result<(), String> {
    use futures_util::StreamExt as _;
    let mut body = std::pin::pin!(body);
    let mut written = 0u64;
    let mut flushed = 0u64;
    while let Some(item) = body.next().await {
        let chunk = item.map_err(|err| err.to_string())?;
        // A failed tee write leaves the hash short of these bytes, so commit refuses the blob at
        // the end; progress only advances on flushed bytes, so tails never read past the file.
        let _ = pending.write(&chunk);
        written += chunk.len() as u64;
        if written - flushed >= FLUSH_EVERY && pending.flush().is_ok() {
            flushed = written;
            sender.send_modify(|progress| progress.flushed = flushed);
        }
    }
    // A failed final flush reads as truncation to any tail and as a digest shortfall at commit.
    let _ = pending.flush();
    sender.send_modify(|progress| progress.flushed = written);
    Ok(())
}

/// One client's view of a live download while it tails the pump's temp file.
struct Tail {
    state: Arc<ServingState>,
    digest: Digest,
    handle: DownloadHandle,
    file: Option<tokio::fs::File>,
    sent: u64,
    route: String,
    filename: String,
    alive: bool,
}

/// Stream a live download to one client by tailing the temp file as the pump flushes it.
pub fn tail_download(
    state: Arc<ServingState>,
    digest: Digest,
    handle: DownloadHandle,
    route: String,
    filename: String,
) -> futures_util::stream::BoxStream<'static, Result<Bytes, std::io::Error>> {
    use futures_util::StreamExt as _;
    use tokio::io::AsyncReadExt as _;
    let tail = Tail {
        state,
        digest,
        handle,
        file: None,
        sent: 0,
        route,
        filename,
        alive: true,
    };
    futures_util::stream::unfold(tail, |mut tail| async move {
        if !tail.alive {
            return None;
        }
        loop {
            let progress = tail.handle.progress().borrow_and_update().clone();
            if tail.sent < progress.flushed {
                if tail.file.is_none() {
                    match tail_file(&tail.state, &mut tail.handle, &tail.digest).await {
                        Ok(file) => tail.file = Some(file),
                        Err(err) => {
                            tail.alive = false;
                            return Some((Err(err), tail));
                        }
                    }
                }
                let reader = tail.file.as_mut().expect("opened above");
                let budget = usize::try_from((progress.flushed - tail.sent).min(FLUSH_EVERY)).expect("bounded chunk");
                let mut buffer = vec![0u8; budget];
                match reader.read(&mut buffer).await {
                    Ok(0) | Err(_) => {
                        tail.alive = false;
                        return Some((Err(std::io::Error::other("blob temp file truncated mid-tail")), tail));
                    }
                    Ok(count) => {
                        buffer.truncate(count);
                        tail.sent += count as u64;
                        return Some((Ok(Bytes::from(buffer)), tail));
                    }
                }
            }
            match progress.done {
                Some(Ok(())) => {
                    tail.state.metrics.record(Event::Download {
                        route: tail.route.clone(),
                        project: project_of_filename(&tail.filename),
                        filename: tail.filename.clone(),
                        bytes: tail.sent,
                    });
                    return None;
                }
                Some(Err(message)) => {
                    tail.alive = false;
                    return Some((Err(std::io::Error::other(message)), tail));
                }
                None => {
                    if tail.handle.progress().changed().await.is_err() {
                        tail.alive = false;
                        return Some((Err(std::io::Error::other("blob transfer abandoned")), tail));
                    }
                }
            }
        }
    })
    .boxed()
}

/// Open the file a tail reads from. The first read always happens at offset zero, so no seek is
/// needed: either the temp file still exists, or the transfer already committed and the blob
/// serves from its final path.
///
/// The commit renames the temp file before the verdict broadcasts, so a missing file with no
/// verdict yet is a normal in-between state: wait for the next progress change and look again.
async fn tail_file(
    state: &ServingState,
    handle: &mut DownloadHandle,
    digest: &Digest,
) -> Result<tokio::fs::File, std::io::Error> {
    loop {
        let missing = match tokio::fs::File::open(handle.path()).await {
            Ok(file) => return Ok(file),
            Err(err) => err,
        };
        let verdict = handle.progress().borrow_and_update().done.clone();
        match verdict {
            Some(Ok(())) => return tokio::fs::File::open(state.blobs.path_for(digest)).await,
            Some(Err(message)) => return Err(std::io::Error::other(message)),
            None => {
                if handle.progress().changed().await.is_err() {
                    return Err(missing);
                }
            }
        }
    }
}
