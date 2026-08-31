use std::sync::{Arc, Mutex};

use futures_util::StreamExt as _;
use peryx_core::{NodeRole, TopologyConfig};
use peryx_driver::download::{DownloadHandle, DownloadProgress};
use peryx_driver::state::{AppState, ServingState};
use peryx_ha::{
    AnalyticsCompleteness, BlobAvailability, BlobAvailabilityError, BlobAvailabilityFailure, BlobServices,
    BlobWriteDurability, CommittedBlob, CompletenessError, CompletenessQuery, CompletenessReport, WriteDurability,
};
use peryx_storage::blob::{BlobStore, BlobTail, Digest, WriteEvidence};
use peryx_storage::meta::MetaStore;
use rstest::rstest;
use tokio::sync::watch;
use tracing::instrument::WithSubscriber as _;

use super::*;
use crate::tests::http::harness;

#[tokio::test]
async fn test_fill_remote_degrades_an_availability_failure() {
    let dir = tempfile::tempdir().unwrap();
    let unavailable = Arc::new(UnavailableAvailability);
    let mut app = local_app(&dir);
    app.install_distributed_availability(peryx_ha::AvailabilityStateInstall {
        role: NodeRole::Replica,
        topology: TopologyConfig::default(),
        blobs: BlobServices::new(Some(unavailable.clone()), unavailable.clone()),
        analytics: unavailable,
        capabilities: peryx_ha::AvailabilityCapabilities::default(),
        authority_drainer: None,
        operations: None,
    })
    .unwrap();
    let state = app.serving;
    let digest = Digest::of(b"remote");
    let capture = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(capture.clone())
        .finish();

    let remote = fill_remote(&state, &digest).with_subscriber(subscriber).await;
    let durability = state
        .confirm_blob_write(CommittedBlob::new(
            &digest,
            b"remote".len() as u64,
            "pypi",
            peryx_ha::AuthorityEpoch(1),
            None,
            WriteEvidence::NodeLocal,
        ))
        .await;
    let completeness = state.analytics_completeness().unwrap().assess(
        &state.meta,
        &[],
        &CompletenessQuery {
            from_day: 1,
            to_day: 1,
            today: 1,
            repository: None,
        },
    );

    assert_eq!(
        (
            remote,
            durability,
            completeness.is_err(),
            capture.text().contains("remote placement read-through failed"),
        ),
        (None, WriteDurability::Unavailable, true, true),
    );
}

#[tokio::test]
async fn test_pump_download_commits_and_reports_the_transfer() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_app(&dir).serving;
    let artifact = Bytes::from_static(b"wheel");
    let digest = Digest::of(&artifact);
    let pending = state.blobs.begin().await.unwrap();
    let (mut handle, producer) = state.downloads.register(digest.as_str(), pending.tail()).unwrap();
    let capture = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(capture.clone())
        .finish();

    pump_download(
        state.clone(),
        digest.clone(),
        futures_util::stream::once(async move { Ok::<_, peryx_upstream::UpstreamError>(artifact) }),
        pending,
        producer,
        ("pypi".to_owned(), "flask-1.0.whl".to_owned(), Some("mirror".to_owned())),
        peryx_driver::rate_limit::UpstreamPermit::default(),
    )
    .with_subscriber(subscriber)
    .await;

    assert_eq!(
        (
            state.blobs.head(&digest).await.unwrap().map(|metadata| metadata.bytes),
            handle.progress().borrow_and_update().done.clone(),
            capture.text().contains("blob transfer ended"),
        ),
        (Some(5), Some(Ok(())), true),
    );
}

#[rstest]
#[case::failed(TailVerdict::Failed, std::io::ErrorKind::Other, Some("upstream reset"))]
#[case::closed(TailVerdict::Closed, std::io::ErrorKind::NotFound, None)]
#[tokio::test]
async fn test_tail_file_returns_the_terminal_error(
    #[case] verdict: TailVerdict,
    #[case] expected_kind: std::io::ErrorKind,
    #[case] expected_message: Option<&str>,
) {
    let dir = tempfile::tempdir().unwrap();
    let state = local_app(&dir).serving;
    let digest = Digest::of(b"missing");
    let pending = state.blobs.begin().await.unwrap();
    let tail = pending.tail().unwrap();
    pending.abort().await.unwrap();
    let mut handle = match verdict {
        TailVerdict::Failed => {
            let (handle, producer) = state.downloads.register(digest.as_str(), tail).unwrap();
            producer.finish(Err("upstream reset".to_owned()));
            handle
        }
        TailVerdict::Closed => {
            let (sender, receiver) = watch::channel(DownloadProgress::default());
            drop(sender);
            DownloadHandle::new(tail, receiver)
        }
    };

    let error = tail_file(&state, &mut handle, &digest).await.unwrap_err();

    assert_eq!(
        (error.kind(), expected_message.map(|_| error.to_string()),),
        (expected_kind, expected_message.map(str::to_owned)),
    );
}

fn local_app(dir: &tempfile::TempDir) -> AppState {
    AppState::new(
        MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        BlobStore::new(dir.path().join("blobs")),
        60,
        Vec::new(),
    )
}

#[derive(Clone, Copy)]
enum TailVerdict {
    Failed,
    Closed,
}

struct UnavailableAvailability;

#[async_trait::async_trait]
impl BlobAvailability for UnavailableAvailability {
    async fn ensure_local(&self, _digest: &Digest) -> Result<Option<BlobMetadata>, BlobAvailabilityError> {
        Err(BlobAvailabilityError::new(
            BlobAvailabilityFailure::Transfer,
            std::io::Error::other("remote unavailable"),
        ))
    }
}

#[async_trait::async_trait]
impl BlobWriteDurability for UnavailableAvailability {
    async fn confirm(&self, _write: CommittedBlob<'_>) -> WriteDurability {
        WriteDurability::Unavailable
    }
}

impl AnalyticsCompleteness for UnavailableAvailability {
    fn assess(
        &self,
        _store: &dyn peryx_ha::AnalyticsSnapshotStore,
        _expected: &[peryx_ha::ExpectedProducer],
        _query: &CompletenessQuery,
    ) -> Result<CompletenessReport, CompletenessError> {
        Err(CompletenessError)
    }
}

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    fn text(&self) -> String {
        std::io::Write::flush(&mut self.clone()).unwrap();
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

impl std::io::Write for Capture {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for Capture {
    type Writer = Self;

    fn make_writer(&'writer self) -> Self::Writer {
        self.clone()
    }
}

fn handle_with(
    tail: BlobTail,
    progress: DownloadProgress,
) -> (DownloadHandle, tokio::sync::watch::Sender<DownloadProgress>) {
    let (sender, receiver) = tokio::sync::watch::channel(progress);
    (DownloadHandle::new(tail, receiver), sender)
}

async fn missing_tail(state: &ServingState) -> BlobTail {
    let pending = state.blobs.begin().await.unwrap();
    let tail = pending.tail().unwrap();
    pending.abort().await.unwrap();
    tail
}

async fn drain(state: &Arc<ServingState>, digest: Digest, handle: DownloadHandle) -> Result<Vec<u8>, std::io::Error> {
    let mut stream = tail_download(state.clone(), digest, handle, "pypi".to_owned(), "tail.whl".to_owned());
    let mut body = Vec::new();
    while let Some(item) = stream.next().await {
        body.extend_from_slice(&item?);
    }
    Ok(body)
}

#[tokio::test]
async fn test_tail_of_a_truncated_temp_file_errors() {
    let h = harness().await;
    let mut pending = h.state.serving.blobs.begin().await.unwrap();
    pending.write_chunk(Bytes::from_static(b"abc")).await.unwrap();
    pending.flush().await.unwrap();
    let progress = DownloadProgress {
        flushed: 100,
        done: None,
    };
    let (handle, sender) = handle_with(pending.tail().unwrap(), progress);
    let mut stream = tail_download(
        h.state.serving.clone(),
        Digest::of(b"tail-target"),
        handle,
        "pypi".to_owned(),
        "tail.whl".to_owned(),
    );
    assert_eq!(stream.next().await.unwrap().unwrap(), Bytes::from_static(b"abc"));
    let err = stream.next().await.unwrap().unwrap_err();
    drop(sender);
    assert!(err.to_string().contains("truncated"));
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn test_tail_switches_to_the_committed_blob_when_the_temp_file_is_gone() {
    let h = harness().await;
    let body = b"committed while attaching";
    let digest = Digest::of(body);
    h.state.serving.blobs.put_bytes_as(body, &digest).await.unwrap();
    let progress = DownloadProgress {
        flushed: body.len() as u64,
        done: Some(Ok(())),
    };
    let (handle, sender) = handle_with(missing_tail(&h.state.serving).await, progress);
    let mut stream = tail_download(
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
    drop(sender);
    assert_eq!(streamed, body);
}

#[tokio::test]
async fn test_committed_tail_holds_its_materialized_lease_until_eof() {
    let h = harness().await;
    let body = b"committed while attaching";
    let digest = Digest::of(body);
    h.state.serving.blobs.put_bytes_as(body, &digest).await.unwrap();
    let progress = DownloadProgress {
        flushed: body.len() as u64,
        done: Some(Ok(())),
    };
    let (handle, sender) = handle_with(missing_tail(&h.state.serving).await, progress);
    let mut stream = tail_download(
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
    drop(sender);
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
    let digest = h.state.serving.blobs.put_bytes(body).await.unwrap();
    let (sender, receiver) = tokio::sync::watch::channel(DownloadProgress {
        flushed: body.len() as u64,
        done: Some(Ok(())),
    });
    let streamed = drain(&h.state.serving, digest, DownloadHandle::new(None, receiver))
        .await
        .unwrap();
    drop(sender);
    assert_eq!(streamed, body);
    h.state.serving.metrics.flush().unwrap();
    let totals = h.state.serving.metrics.index_totals();
    let pypi = totals.get("pypi").expect("pypi counters present after settle");
    assert_eq!(pypi.base.reads, 1);
    assert_eq!(pypi.base.bytes, body.len() as u64);
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
    let (sender, receiver) = tokio::sync::watch::channel(progress);
    let handle = DownloadHandle::new(missing_tail(&h.state.serving).await, receiver);
    let draining = drain(&h.state.serving, digest.clone(), handle);
    tokio::pin!(draining);
    assert!(futures_util::poll!(draining.as_mut()).is_pending());
    h.state.serving.blobs.put_bytes_as(body, &digest).await.unwrap();
    sender.send_modify(|progress| progress.done = Some(Ok(())));
    let streamed = draining.await.unwrap();
    assert_eq!(streamed, body);
}

#[tokio::test]
async fn test_tail_with_a_missing_temp_file_surfaces_the_failure_verdict() {
    let h = harness().await;
    let progress = DownloadProgress {
        flushed: 10,
        done: Some(Err("verification failed".to_owned())),
    };
    let (handle, sender) = handle_with(missing_tail(&h.state.serving).await, progress);
    let err = drain(&h.state.serving, Digest::of(b"tail-target"), handle)
        .await
        .unwrap_err();
    drop(sender);
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
    let handle = DownloadHandle::new(missing_tail(&h.state.serving).await, receiver);
    let err = drain(&h.state.serving, Digest::of(b"tail-target"), handle)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}

#[tokio::test]
async fn test_tail_surfaces_the_transfer_failure() {
    let h = harness().await;
    let progress = DownloadProgress {
        flushed: 0,
        done: Some(Err("upstream fell over".to_owned())),
    };
    let (handle, sender) = handle_with(missing_tail(&h.state.serving).await, progress);
    let err = drain(&h.state.serving, Digest::of(b"tail-target"), handle)
        .await
        .unwrap_err();
    drop(sender);
    assert!(err.to_string().contains("upstream fell over"));
}

#[tokio::test]
async fn test_tail_errors_when_the_pump_vanishes_without_a_verdict() {
    let h = harness().await;
    let (sender, receiver) = tokio::sync::watch::channel(DownloadProgress::default());
    drop(sender);
    let handle = DownloadHandle::new(missing_tail(&h.state.serving).await, receiver);
    let err = drain(&h.state.serving, Digest::of(b"tail-target"), handle)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("abandoned"));
}
