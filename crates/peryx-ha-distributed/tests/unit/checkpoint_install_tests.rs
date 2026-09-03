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
