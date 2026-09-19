//! A replica below a source's floor recovering through a checkpoint transfer.

use std::num::NonZeroUsize;
use std::sync::Mutex;

use async_trait::async_trait;
use peryx_storage::meta::{CheckpointIdentity, CheckpointManifest, MetaStore};

use crate::peer::{BatchFrame, BatchRequest, CheckpointWindow, PeerTransport};
use crate::protocol::PROTOCOL_VERSION;
use crate::{Replica, SyncError, TransportError};

const ONE: NonZeroUsize = NonZeroUsize::new(1).expect("1 is non-zero");
const SOURCE: &str = "primary-a";
const CHUNK: usize = 256;

fn identity() -> CheckpointIdentity {
    CheckpointIdentity {
        source: SOURCE.to_owned(),
        protocol_version: PROTOCOL_VERSION,
        schema_version: 1,
    }
}

fn store(dir: &tempfile::TempDir, name: &str) -> MetaStore {
    MetaStore::open(dir.path().join(name)).unwrap()
}

/// Writes `rows` driver rows through the ordinary path, so the journal holds what a real write leaves.
fn rows(meta: &MetaStore, count: usize) {
    for index in 0..count {
        meta.commit_driver_txn(|txn| {
            txn.put(&format!("pypi\u{0}p\u{0}hosted/pkg{index:04}"), b"display")
                .map(|()| ((), vec![b"{}".to_vec()]))
        })
        .unwrap();
    }
}

/// Serves the checkpoint a store publishes, optionally failing after a number of windows.
///
/// The failure is counted rather than timed, so the interruption lands at the same chunk boundary on
/// every run.
struct CheckpointPeer {
    meta: MetaStore,
    windows_before_loss: Mutex<Option<usize>>,
    /// Flips a byte in the first window that carries any, so the transfer completes and fails its digest.
    corrupt: bool,
}

impl CheckpointPeer {
    fn serving(meta: MetaStore) -> Self {
        Self {
            meta,
            windows_before_loss: Mutex::new(None),
            corrupt: false,
        }
    }

    fn losing_after(meta: MetaStore, windows: usize) -> Self {
        Self {
            meta,
            windows_before_loss: Mutex::new(Some(windows)),
            corrupt: false,
        }
    }

    fn corrupting(meta: MetaStore) -> Self {
        Self {
            meta,
            windows_before_loss: Mutex::new(None),
            corrupt: true,
        }
    }
}

#[async_trait]
impl PeerTransport for CheckpointPeer {
    async fn fetch_batch(&self, _request: BatchRequest) -> Result<BatchFrame, TransportError> {
        Err(TransportError::CheckpointRequired)
    }

    async fn checkpoint_manifest(&self) -> Result<CheckpointManifest, TransportError> {
        self.meta
            .checkpoint_manifest()
            .map_err(|_| TransportError::Malformed)?
            .ok_or(TransportError::CheckpointUnavailable)
    }

    async fn checkpoint_chunk(&self, cursor: &str) -> Result<CheckpointWindow, TransportError> {
        let mut budget = self.windows_before_loss.lock().expect("the counter is usable");
        if let Some(remaining) = budget.as_mut() {
            if *remaining == 0 {
                return Err(TransportError::Disconnected);
            }
            *remaining -= 1;
        }
        drop(budget);
        let cursor = peryx_storage::meta::CheckpointCursor::from_token(cursor).ok_or(TransportError::Malformed)?;
        let chunk = self
            .meta
            .checkpoint_chunk(&cursor, CHUNK)
            .map_err(|_| TransportError::Malformed)?;
        let mut bytes = chunk.bytes;
        if self.corrupt && !bytes.is_empty() {
            bytes[0] ^= 0xff;
        }
        Ok(CheckpointWindow {
            bytes,
            next: chunk.next.token(),
        })
    }
}

/// The change feed every checkpoint-serving double refuses, which is what sends a reader to a transfer.
async fn refused_feed<T: PeerTransport>(peer: &T) -> TransportError {
    peer.fetch_batch(BatchRequest {
        after: 0,
        max_operations: ONE,
    })
    .await
    .expect_err("a source below the floor refuses the feed")
}

fn published(meta: &MetaStore) -> CheckpointManifest {
    meta.publish_checkpoint(identity()).unwrap()
}

#[tokio::test]
async fn test_a_replica_below_the_floor_installs_and_stands_at_the_manifest_serial() {
    let dir = tempfile::tempdir().unwrap();
    let writer = store(&dir, "writer.redb");
    rows(&writer, 12);
    let manifest = published(&writer);
    let replica = store(&dir, "replica.redb");

    let peer = CheckpointPeer::serving(writer.clone());
    assert_eq!(refused_feed(&peer).await, TransportError::CheckpointRequired);

    let serial = Replica::new(&replica, ONE)
        .install_checkpoint(&peer, SOURCE)
        .await
        .unwrap();

    assert_eq!(serial, manifest.serial);
    assert_eq!(replica.current_serial().unwrap(), manifest.serial);
    let state = Replica::new(&replica, ONE).state().unwrap().unwrap();
    assert_eq!((state.source, state.serial), (SOURCE.to_owned(), manifest.serial));
    assert_eq!(
        replica.get_driver_value("pypi\u{0}p\u{0}hosted/pkg0000").unwrap(),
        writer.get_driver_value("pypi\u{0}p\u{0}hosted/pkg0000").unwrap()
    );
}

/// The serial a replica resumes from is the one it installed. A floor that advanced between the refusal
/// and the install would otherwise leave its cursor and its state at different serials, which no later
/// page reveals.
#[tokio::test]
async fn test_the_resume_serial_comes_from_the_manifest_that_was_installed() {
    let dir = tempfile::tempdir().unwrap();
    let writer = store(&dir, "writer.redb");
    rows(&writer, 4);
    let refused_at = published(&writer);
    // The source moves on and republishes between the refusal and the transfer.
    rows(&writer, 4);
    let installed_at = published(&writer);
    assert!(installed_at.serial > refused_at.serial);
    let replica = store(&dir, "replica.redb");

    let serial = Replica::new(&replica, ONE)
        .install_checkpoint(&CheckpointPeer::serving(writer.clone()), SOURCE)
        .await
        .unwrap();

    assert_eq!(
        (serial, replica.current_serial().unwrap()),
        (installed_at.serial, installed_at.serial)
    );
}

#[tokio::test]
async fn test_an_interrupted_install_leaves_the_previous_state_usable_and_a_restart_completes() {
    let dir = tempfile::tempdir().unwrap();
    let writer = store(&dir, "writer.redb");
    rows(&writer, 40);
    let manifest = published(&writer);
    let replica = store(&dir, "replica.redb");
    rows(&replica, 1);
    let before = replica.current_serial().unwrap();

    let interrupted = Replica::new(&replica, ONE)
        .install_checkpoint(&CheckpointPeer::losing_after(writer.clone(), 2), SOURCE)
        .await
        .unwrap_err();

    assert!(matches!(interrupted, SyncError::Primary(_)), "{interrupted:?}");
    assert_eq!(replica.current_serial().unwrap(), before);
    assert_eq!(
        replica
            .get_driver_value("pypi\u{0}p\u{0}hosted/pkg0000")
            .unwrap()
            .as_deref(),
        Some(&b"display"[..])
    );
    let staged = replica.staged_checkpoint().unwrap().unwrap();
    assert!(staged.received > 0 && staged.received < manifest.bytes);

    let serial = Replica::new(&replica, ONE)
        .install_checkpoint(&CheckpointPeer::serving(writer.clone()), SOURCE)
        .await
        .unwrap();

    assert_eq!(serial, manifest.serial);
    assert_eq!(replica.staged_checkpoint().unwrap(), None);
}

/// A restart with the same manifest must pick up from the staged cursor rather than discard it and
/// start over, or an interruption near the end of a large transfer would refetch it whole every retry.
#[tokio::test]
async fn test_a_restart_resumes_from_the_staged_cursor_not_from_the_beginning() {
    struct RecordingCursors {
        inner: CheckpointPeer,
        cursors: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl PeerTransport for RecordingCursors {
        async fn fetch_batch(&self, request: BatchRequest) -> Result<BatchFrame, TransportError> {
            self.inner.fetch_batch(request).await
        }

        async fn checkpoint_manifest(&self) -> Result<CheckpointManifest, TransportError> {
            self.inner.checkpoint_manifest().await
        }

        async fn checkpoint_chunk(&self, cursor: &str) -> Result<CheckpointWindow, TransportError> {
            self.cursors
                .lock()
                .expect("the recorder is usable")
                .push(cursor.to_owned());
            self.inner.checkpoint_chunk(cursor).await
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let writer = store(&dir, "writer.redb");
    rows(&writer, 40);
    published(&writer);
    let replica = store(&dir, "replica.redb");
    Replica::new(&replica, ONE)
        .install_checkpoint(&CheckpointPeer::losing_after(writer.clone(), 2), SOURCE)
        .await
        .unwrap_err();
    let staged = replica.staged_checkpoint().unwrap().unwrap();
    assert!(staged.received > 0, "the test needs a partial transfer to resume from");

    let peer = RecordingCursors {
        inner: CheckpointPeer::serving(writer.clone()),
        cursors: Mutex::new(Vec::new()),
    };
    assert_eq!(refused_feed(&peer).await, TransportError::CheckpointRequired);
    Replica::new(&replica, ONE)
        .install_checkpoint(&peer, SOURCE)
        .await
        .unwrap();

    let first_cursor_requested = peer.cursors.lock().unwrap().first().cloned().unwrap();
    assert_eq!(
        first_cursor_requested, staged.cursor,
        "a restart resumes from the staged cursor, not the beginning"
    );
}

/// The retry peer refuses a chunk requested from the very start, so this only passes if the installer
/// resumed from the staged cursor rather than reopening the transfer.
struct RefusingFromScratch(MetaStore);

#[async_trait]
impl PeerTransport for RefusingFromScratch {
    async fn fetch_batch(&self, _request: BatchRequest) -> Result<BatchFrame, TransportError> {
        Err(TransportError::CheckpointRequired)
    }

    async fn checkpoint_manifest(&self) -> Result<CheckpointManifest, TransportError> {
        self.0
            .checkpoint_manifest()
            .map_err(|_| TransportError::Malformed)?
            .ok_or(TransportError::CheckpointUnavailable)
    }

    async fn checkpoint_chunk(&self, cursor: &str) -> Result<CheckpointWindow, TransportError> {
        if cursor == peryx_storage::meta::CheckpointCursor::start().token() {
            return Err(TransportError::Disconnected);
        }
        let cursor = peryx_storage::meta::CheckpointCursor::from_token(cursor).ok_or(TransportError::Malformed)?;
        let chunk = self
            .0
            .checkpoint_chunk(&cursor, CHUNK)
            .map_err(|_| TransportError::Malformed)?;
        Ok(CheckpointWindow {
            bytes: chunk.bytes,
            next: chunk.next.token(),
        })
    }
}

#[tokio::test]
async fn test_a_matching_staged_manifest_resumes_rather_than_restarting() {
    let dir = tempfile::tempdir().unwrap();
    let writer = store(&dir, "writer.redb");
    rows(&writer, 40);
    let manifest = published(&writer);
    let replica = store(&dir, "replica.redb");

    Replica::new(&replica, ONE)
        .install_checkpoint(&CheckpointPeer::losing_after(writer.clone(), 2), SOURCE)
        .await
        .unwrap_err();
    let staged = replica.staged_checkpoint().unwrap().unwrap();
    assert!(staged.received > 0 && staged.received < manifest.bytes);

    let peer = RefusingFromScratch(writer.clone());
    assert_eq!(refused_feed(&peer).await, TransportError::CheckpointRequired);
    let serial = Replica::new(&replica, ONE)
        .install_checkpoint(&peer, SOURCE)
        .await
        .unwrap();

    assert_eq!(serial, manifest.serial);
}

#[tokio::test]
async fn test_a_corrupted_checkpoint_is_rejected_and_does_not_replace_live_state() {
    let dir = tempfile::tempdir().unwrap();
    let writer = store(&dir, "writer.redb");
    rows(&writer, 6);
    published(&writer);
    let replica = store(&dir, "replica.redb");
    rows(&replica, 1);
    let before = replica.current_serial().unwrap();

    let refused = Replica::new(&replica, ONE)
        .install_checkpoint(&CheckpointPeer::corrupting(writer.clone()), SOURCE)
        .await
        .unwrap_err();

    assert!(matches!(refused, SyncError::Checkpoint(_)), "{refused:?}");
    assert_eq!(replica.current_serial().unwrap(), before);
    assert_eq!(
        replica
            .get_driver_value("pypi\u{0}p\u{0}hosted/pkg0000")
            .unwrap()
            .as_deref(),
        Some(&b"display"[..])
    );
}

/// A peer that keeps claiming more data is available after every byte the manifest promises has already
/// arrived. The transfer must stop on byte count alone rather than trust that claim, or a misbehaving
/// peer could keep it fetching forever.
#[tokio::test]
async fn test_the_transfer_stops_once_every_byte_arrives_even_if_the_peer_claims_more() {
    struct ClaimsMoreAfterEveryByteArrives {
        payload: Vec<u8>,
        manifest: CheckpointManifest,
        calls: Mutex<usize>,
    }

    #[async_trait]
    impl PeerTransport for ClaimsMoreAfterEveryByteArrives {
        async fn fetch_batch(&self, _request: BatchRequest) -> Result<BatchFrame, TransportError> {
            Err(TransportError::CheckpointRequired)
        }

        async fn checkpoint_manifest(&self) -> Result<CheckpointManifest, TransportError> {
            Ok(self.manifest.clone())
        }

        async fn checkpoint_chunk(&self, _cursor: &str) -> Result<CheckpointWindow, TransportError> {
            let mut calls = self.calls.lock().expect("the counter is usable");
            *calls += 1;
            if *calls == 1 {
                Ok(CheckpointWindow {
                    bytes: self.payload.clone(),
                    next: "not-actually-done".to_owned(),
                })
            } else {
                Err(TransportError::Disconnected)
            }
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let writer = store(&dir, "writer.redb");
    rows(&writer, 12);
    let manifest = published(&writer);
    let whole = writer
        .checkpoint_chunk(
            &peryx_storage::meta::CheckpointCursor::start(),
            usize::try_from(manifest.bytes).unwrap(),
        )
        .unwrap();
    assert_eq!(
        whole.bytes.len() as u64,
        manifest.bytes,
        "the test needs the whole manifest in one window"
    );
    let replica = store(&dir, "replica.redb");
    let peer = ClaimsMoreAfterEveryByteArrives {
        payload: whole.bytes,
        manifest: manifest.clone(),
        calls: Mutex::new(0),
    };
    assert_eq!(refused_feed(&peer).await, TransportError::CheckpointRequired);

    let serial = Replica::new(&replica, ONE)
        .install_checkpoint(&peer, SOURCE)
        .await
        .unwrap();

    assert_eq!(serial, manifest.serial);
}

#[tokio::test]
async fn test_a_source_publishing_no_checkpoint_reports_it_rather_than_waiting() {
    let dir = tempfile::tempdir().unwrap();
    let writer = store(&dir, "writer.redb");
    rows(&writer, 2);
    let replica = store(&dir, "replica.redb");

    let refused = Replica::new(&replica, ONE)
        .install_checkpoint(&CheckpointPeer::serving(writer.clone()), SOURCE)
        .await
        .unwrap_err();

    assert!(matches!(refused, SyncError::Primary(_)), "{refused:?}");
}

/// A transport that serves no checkpoint at all refuses rather than leaving a reader below the floor
/// waiting on a recovery that cannot arrive.
#[tokio::test]
async fn test_a_transport_without_checkpoint_support_refuses() {
    struct FeedOnly;

    #[async_trait]
    impl PeerTransport for FeedOnly {
        async fn fetch_batch(&self, _request: BatchRequest) -> Result<BatchFrame, TransportError> {
            Err(TransportError::CheckpointRequired)
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let replica = store(&dir, "replica.redb");

    let refused = Replica::new(&replica, ONE)
        .install_checkpoint(&FeedOnly, SOURCE)
        .await
        .unwrap_err();

    assert!(matches!(refused, SyncError::Primary(_)), "{refused:?}");
    assert_eq!(refused_feed(&FeedOnly).await, TransportError::CheckpointRequired);
    assert_eq!(
        FeedOnly.checkpoint_chunk("r").await.unwrap_err(),
        TransportError::CheckpointUnavailable
    );
    assert_eq!(
        TransportError::CheckpointUnavailable.terminal_reason(),
        Some("checkpoint_unavailable")
    );
}

/// A window that would carry the transfer past the length its manifest declares is not a window this
/// transfer can use, so the staging goes and the next attempt starts from the beginning.
#[tokio::test]
async fn test_a_window_that_overruns_the_manifest_drops_the_staging() {
    struct Overrunning(MetaStore);

    #[async_trait]
    impl PeerTransport for Overrunning {
        async fn fetch_batch(&self, _request: BatchRequest) -> Result<BatchFrame, TransportError> {
            Err(TransportError::CheckpointRequired)
        }

        async fn checkpoint_manifest(&self) -> Result<CheckpointManifest, TransportError> {
            self.0
                .checkpoint_manifest()
                .map_err(|_| TransportError::Malformed)?
                .ok_or(TransportError::CheckpointUnavailable)
        }

        async fn checkpoint_chunk(&self, _cursor: &str) -> Result<CheckpointWindow, TransportError> {
            let manifest = self.checkpoint_manifest().await?;
            Ok(CheckpointWindow {
                bytes: vec![0; usize::try_from(manifest.bytes).expect("a test checkpoint fits a pointer") + 1],
                next: "done".to_owned(),
            })
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let writer = store(&dir, "writer.redb");
    rows(&writer, 4);
    published(&writer);
    let replica = store(&dir, "replica.redb");

    let peer = Overrunning(writer.clone());
    assert_eq!(refused_feed(&peer).await, TransportError::CheckpointRequired);
    let refused = Replica::new(&replica, ONE)
        .install_checkpoint(&peer, SOURCE)
        .await
        .unwrap_err();

    assert!(matches!(refused, SyncError::CheckpointChunk(_)), "{refused:?}");
    assert_eq!(replica.staged_checkpoint().unwrap(), None);
}

fn fake_manifest(bytes: u64) -> CheckpointManifest {
    CheckpointManifest {
        identity: identity(),
        serial: 1,
        rows: 0,
        revocations: 0,
        blobs: 0,
        bytes,
        digest: "deadbeef".to_owned(),
    }
}

/// Reports exactly the declared byte count in one window while still pointing past it, so only the
/// byte-count guard can be what stops the transfer.
struct ExactByteCountPeer {
    manifest: CheckpointManifest,
    calls: Mutex<usize>,
}

#[async_trait]
impl PeerTransport for ExactByteCountPeer {
    async fn fetch_batch(&self, _request: BatchRequest) -> Result<BatchFrame, TransportError> {
        Err(TransportError::CheckpointRequired)
    }

    async fn checkpoint_manifest(&self) -> Result<CheckpointManifest, TransportError> {
        Ok(self.manifest.clone())
    }

    async fn checkpoint_chunk(&self, _cursor: &str) -> Result<CheckpointWindow, TransportError> {
        *self.calls.lock().unwrap() += 1;
        Ok(CheckpointWindow {
            bytes: vec![0; usize::try_from(self.manifest.bytes).expect("a test checkpoint fits a pointer")],
            next: peryx_storage::meta::CheckpointCursor::Rows {
                after: Some("more".to_owned()),
            }
            .token(),
        })
    }
}

#[tokio::test]
async fn test_reaching_the_declared_byte_count_stops_the_transfer() {
    let dir = tempfile::tempdir().unwrap();
    let replica = store(&dir, "replica.redb");
    let peer = ExactByteCountPeer {
        manifest: fake_manifest(5),
        calls: Mutex::new(0),
    };
    assert_eq!(refused_feed(&peer).await, TransportError::CheckpointRequired);

    let _ = Replica::new(&replica, ONE).install_checkpoint(&peer, SOURCE).await;

    assert_eq!(
        *peer.calls.lock().unwrap(),
        1,
        "the transfer should stop as soon as it reaches the declared byte count",
    );
}

/// Reports fewer bytes than declared but signals `Done` immediately, so only the cursor guard can be
/// what stops the transfer.
struct ShortDonePeer {
    manifest: CheckpointManifest,
    calls: Mutex<usize>,
}

#[async_trait]
impl PeerTransport for ShortDonePeer {
    async fn fetch_batch(&self, _request: BatchRequest) -> Result<BatchFrame, TransportError> {
        Err(TransportError::CheckpointRequired)
    }

    async fn checkpoint_manifest(&self) -> Result<CheckpointManifest, TransportError> {
        Ok(self.manifest.clone())
    }

    async fn checkpoint_chunk(&self, _cursor: &str) -> Result<CheckpointWindow, TransportError> {
        *self.calls.lock().unwrap() += 1;
        Ok(CheckpointWindow {
            bytes: vec![0; 3],
            next: peryx_storage::meta::CheckpointCursor::Done.token(),
        })
    }
}

#[tokio::test]
async fn test_a_done_cursor_stops_the_transfer_short_of_the_declared_byte_count() {
    let dir = tempfile::tempdir().unwrap();
    let replica = store(&dir, "replica.redb");
    let peer = ShortDonePeer {
        manifest: fake_manifest(5),
        calls: Mutex::new(0),
    };
    assert_eq!(refused_feed(&peer).await, TransportError::CheckpointRequired);

    let _ = Replica::new(&replica, ONE).install_checkpoint(&peer, SOURCE).await;

    assert_eq!(
        *peer.calls.lock().unwrap(),
        1,
        "a done cursor should stop the transfer even short of the declared byte count",
    );
}
