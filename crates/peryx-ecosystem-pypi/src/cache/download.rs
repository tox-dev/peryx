//! File-download streaming: cached blobs, single-flight upstream transfers, and live tailing.

use std::sync::Arc;

use crate::project_of_filename;
use crate::store::PypiStore as _;
use bytes::Bytes;
use peryx_driver::download::{DownloadHandle, DownloadProducer};
use peryx_driver::rate_limit::UpstreamPermit;
use peryx_driver::state::ServingState;
use peryx_events::metrics::Event;
use peryx_storage::blob::{BlobLease, BlobMetadata, BlobWrite, Digest};

use super::{CacheError, flight_gate, release_flight, source_artifact_client, source_client, upstream_permit};

/// Resolve a file to a hosted blob path. A cache miss is fetched through the same streaming path as
/// downloads, so the archive inspector never buffers the whole artifact in memory.
///
/// # Errors
/// Returns [`CacheError::FileNotFound`] if the digest has no known source, or another error on a
/// store or upstream failure.
///
#[expect(
    clippy::significant_drop_tightening,
    reason = "the connect gate stays held across start_download so racing inspectors attach instead of double-fetching"
)]
pub async fn file_path(
    state: Arc<ServingState>,
    digest: Digest,
    route: String,
    filename: String,
) -> Result<BlobLease, CacheError> {
    if state.blobs.head(&digest).await?.is_some() {
        return Ok(state.blobs.materialize(&digest).await?);
    }
    let gate = flight_gate(&state, digest.as_str());
    let guard = gate.lock_owned().await;
    if state.blobs.head(&digest).await?.is_some() {
        release_flight(&state, digest.as_str(), guard);
        return Ok(state.blobs.materialize(&digest).await?);
    }
    let mut handle = if let Some(running) = existing_download(&state, &digest) {
        running
    } else {
        start_download(&state, &digest, route, filename).await?
    };
    release_flight(&state, digest.as_str(), guard);
    wait_for_download(&mut handle).await?;
    Ok(state.blobs.materialize(&digest).await?)
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
pub async fn probe_file(state: &ServingState, digest: &Digest) -> Result<FileProbe, CacheError> {
    // One stat answers "is it cached", sizes it, and dates it, where the streaming path needs the
    // handle too. The date is what a `GET` of the same blob validates on, so a `HEAD` states it too.
    if let Some(blob) = state.blobs.head(digest).await? {
        return Ok(FileProbe::Cached(blob.bytes, blob.modified));
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
    /// Metadata for a stored blob; the caller opens the selected range after evaluating conditions.
    Cached(BlobMetadata),
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
    if let Some(metadata) = state.blobs.head(&digest).await? {
        return Ok(FileOutcome::Cached(metadata));
    }
    // The gate serializes only the connect phase, so an upstream refusal stays a clean HTTP error
    // for every racing client instead of a mid-body abort on a started stream.
    let gate = flight_gate(&state, digest.as_str());
    let guard = gate.lock_owned().await;
    if let Some(metadata) = state.blobs.head(&digest).await? {
        release_flight(&state, digest.as_str(), guard);
        return Ok(FileOutcome::Cached(metadata));
    }
    let handle = if let Some(running) = existing_download(&state, &digest) {
        running
    } else {
        start_download(&state, &digest, route.clone(), filename.clone()).await?
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
    let (client, offline) = source_artifact_client(state, &source.source, source.upstream.as_deref())?;
    if offline {
        return Err(CacheError::OfflineMissing("file"));
    }
    let permit = upstream_permit(state, &source.source).await?;
    let body = client.stream_bytes(&source.url).await?;
    let pending = state.blobs.begin().await?;
    let (handle, producer) = state
        .downloads
        .register(digest.as_str(), pending.tail())
        .expect("connect gate serializes download registration");
    let pump_state = state.clone();
    let pump_digest = digest.clone();
    tokio::spawn(async move {
        pump_download(
            pump_state,
            pump_digest,
            body,
            pending,
            producer,
            (route, filename, source.upstream),
            permit,
        )
        .await;
    });
    Ok(handle)
}

/// The in-flight download for `digest`, if one is pumping right now.
fn existing_download(state: &ServingState, digest: &Digest) -> Option<DownloadHandle> {
    state.downloads.get(digest.as_str())
}

async fn wait_for_download(handle: &mut DownloadHandle) -> Result<(), CacheError> {
    loop {
        let done = handle.progress().borrow_and_update().done.clone();
        match done {
            Some(Ok(())) => return Ok(()),
            Some(Err(message)) => return Err(CacheError::Stream(message)),
            None => {
                handle
                    .progress()
                    .changed()
                    .await
                    .expect("download producer publishes terminal progress");
            }
        }
    }
}

/// Chunks smaller than this batch up before a flush makes them visible to tailing readers.
const FLUSH_EVERY: u64 = 256 * 1024;

fn string_result<T, E: std::fmt::Display>(result: Result<T, E>) -> Result<T, String> {
    match result {
        Ok(value) => Ok(value),
        Err(error) => Err(error.to_string()),
    }
}

/// Pull the upstream body into the pending blob, publishing flushed progress as it lands; on EOF
/// the blob persists only when the digest verifies (clients check their own hashes regardless).
/// Runs detached from any client, so the cache fills even when every requester disconnects.
async fn pump_download(
    state: Arc<ServingState>,
    digest: Digest,
    body: impl futures_util::Stream<Item = Result<Bytes, peryx_upstream::UpstreamError>> + Send + 'static,
    mut pending: BlobWrite,
    producer: DownloadProducer,
    served_as: (String, String, Option<String>),
    permit: UpstreamPermit,
) {
    let started = std::time::Instant::now();
    let outcome = match drain_to_blob(body, &mut pending, &producer).await {
        Ok(()) => pending.commit(&digest).await.map_err(|err| err.to_string()),
        Err(error) => match pending.abort().await {
            Ok(()) => Err(error),
            Err(cleanup) => Err(cleanup.to_string()),
        },
    };
    drop(permit);
    let bytes = producer.flushed();
    let elapsed_ms = started.elapsed().as_millis();
    let (route, filename, upstream) = served_as;
    let upstream = upstream.as_deref().unwrap_or("");
    #[rustfmt::skip]
    tracing::debug!(digest = digest.as_str(), upstream, bytes, elapsed_ms, "blob transfer ended");
    if outcome.is_err() {
        tracing::warn!(digest = digest.as_str(), "blob persist rejected");
        let project = project_of_filename(&filename);
        state.metrics.record(Event::BlobRejected { route, project });
    }
    producer.finish(outcome);
}

/// Tee the upstream body into the pending blob, publishing progress at every flush. Errors map to
/// strings so the watch channel can carry the verdict to every tailing client.
async fn drain_to_blob(
    body: impl futures_util::Stream<Item = Result<Bytes, peryx_upstream::UpstreamError>> + Send + 'static,
    pending: &mut BlobWrite,
    producer: &DownloadProducer,
) -> Result<(), String> {
    use futures_util::StreamExt as _;
    let mut body = std::pin::pin!(body);
    let mut written = 0u64;
    let mut flushed = 0u64;
    while let Some(item) = body.next().await {
        let chunk = string_result(item)?;
        let chunk_len = chunk.len() as u64;
        string_result(pending.write_chunk(chunk).await)?;
        written += chunk_len;
        if written - flushed >= FLUSH_EVERY {
            flushed = string_result(pending.flush().await)?;
            producer.publish_flushed(flushed);
        }
    }
    let flushed = string_result(pending.flush().await)?;
    producer.publish_flushed(flushed);
    Ok(())
}

/// One client's view of a live download while it tails the pump's temp file.
struct Tail {
    state: Arc<ServingState>,
    digest: Digest,
    handle: DownloadHandle,
    file: Option<tokio::fs::File>,
    lease: Option<BlobLease>,
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
    if handle.tail().is_none() {
        return committed_download(state, digest, handle, route, filename);
    }
    let tail = Tail {
        state,
        digest,
        handle,
        file: None,
        lease: None,
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
                        Ok((file, lease)) => {
                            tail.file = Some(file);
                            tail.lease = lease;
                        }
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
                    let (version, source) = download_dimensions(&tail.state, &tail.digest, &tail.filename);
                    tail.state.metrics.record(Event::Download {
                        route: tail.route.clone(),
                        project: project_of_filename(&tail.filename),
                        filename: tail.filename.clone(),
                        version,
                        source,
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

/// The version and routed-source labels for a served artifact's daily-usage bucket.
///
/// `version` comes from the filename alone. `source` is the upstream the blob was routed from, read
/// from the local store: a hosted upload and any blob with no recorded upstream both yield `None`, so
/// a response served without routing to an upstream carries no source. The read is local and off the
/// cross-node path, so it stays within the collection boundary's constraints.
pub fn download_dimensions(state: &ServingState, digest: &Digest, filename: &str) -> (Option<String>, Option<String>) {
    let version = crate::distribution_version_segment(filename).map(str::to_owned);
    let source = state
        .meta
        .get_file_url(digest.as_str())
        .ok()
        .flatten()
        .and_then(|source| source.upstream);
    (version, source)
}

fn committed_download(
    state: Arc<ServingState>,
    digest: Digest,
    mut handle: DownloadHandle,
    route: String,
    filename: String,
) -> futures_util::stream::BoxStream<'static, Result<Bytes, std::io::Error>> {
    use futures_util::{StreamExt as _, TryStreamExt as _};
    futures_util::stream::once(async move {
        wait_for_download(&mut handle).await.map_err(std::io::Error::other)?;
        let read = state.blobs.open(&digest, None).await.map_err(std::io::Error::other)?;
        let stream = peryx_driver::body::blob_read(read)
            .into_data_stream()
            .map_err(std::io::Error::other)
            .boxed();
        Ok::<_, std::io::Error>(
            futures_util::stream::try_unfold(
                (stream, state, digest, route, filename, 0u64),
                |(mut stream, state, digest, route, filename, bytes)| async move {
                    let Some(chunk) = stream.next().await.transpose()? else {
                        let (version, source) = download_dimensions(&state, &digest, &filename);
                        state.metrics.record(Event::Download {
                            route,
                            project: project_of_filename(&filename),
                            filename,
                            version,
                            source,
                            bytes,
                        });
                        return Ok(None);
                    };
                    let bytes = bytes.saturating_add(chunk.len() as u64);
                    Ok(Some((chunk, (stream, state, digest, route, filename, bytes))))
                },
            )
            .boxed(),
        )
    })
    .try_flatten()
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
) -> Result<(tokio::fs::File, Option<BlobLease>), std::io::Error> {
    loop {
        let tail = handle.tail().expect("tailing download retains a local tail").clone();
        let missing = match tokio::task::spawn_blocking(move || tail.open())
            .await
            .map_err(std::io::Error::other)?
        {
            Ok(file) => return Ok((tokio::fs::File::from_std(file), None)),
            Err(err) => err,
        };
        let verdict = handle.progress().borrow_and_update().done.clone();
        match verdict {
            Some(Ok(())) => {
                let lease = state.blobs.materialize(digest).await.map_err(std::io::Error::other)?;
                let file = std::fs::File::open(lease.path()).map_err(std::io::Error::other)?;
                return Ok((tokio::fs::File::from_std(file), Some(lease)));
            }
            Some(Err(message)) => return Err(std::io::Error::other(message)),
            None => {
                if handle.progress().changed().await.is_err() {
                    return Err(missing);
                }
            }
        }
    }
}
