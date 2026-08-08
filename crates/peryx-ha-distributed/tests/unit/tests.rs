use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::sync::Mutex;

use async_trait::async_trait;
use peryx_storage::blob::{BlobStorage, Digest};
use peryx_storage::meta::MetaStore;

use crate::{BlobReference, Change, ChangePage, MetadataMutation, PROTOCOL_VERSION, Primary, Replica, SyncError};

#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
struct PrimaryError(String);

struct TestPrimary {
    pages: BTreeMap<u64, ChangePage>,
    requests: Mutex<Vec<u64>>,
}

#[async_trait]
impl Primary for TestPrimary {
    type Error = PrimaryError;

    async fn changes(&self, after: u64, _limit: usize) -> Result<ChangePage, Self::Error> {
        self.requests.lock().unwrap().push(after);
        self.pages
            .get(&after)
            .cloned()
            .ok_or_else(|| PrimaryError(format!("no page after {after}")))
    }
}

fn stores() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, meta)
}

fn primary(pages: Vec<ChangePage>) -> TestPrimary {
    TestPrimary {
        pages: pages.into_iter().map(|page| (page.after, page)).collect(),
        requests: Mutex::default(),
    }
}

fn primary_at(after: u64, page: ChangePage) -> TestPrimary {
    TestPrimary {
        pages: BTreeMap::from([(after, page)]),
        requests: Mutex::default(),
    }
}

fn page(source: &str, after: u64, current_serial: u64, changes: Vec<Change>) -> ChangePage {
    ChangePage {
        version: PROTOCOL_VERSION,
        source: source.to_owned(),
        after,
        current_serial,
        changes,
    }
}

fn change(serial: u64, metadata: Vec<MetadataMutation>, blobs: Vec<BlobReference>) -> Change {
    Change {
        serial,
        event: format!("event-{serial}").into_bytes(),
        metadata,
        blobs,
    }
}

fn put(key: &str, value: &[u8]) -> MetadataMutation {
    MetadataMutation::Put {
        key: key.to_owned(),
        value: value.to_vec(),
    }
}

fn delete(key: &str) -> MetadataMutation {
    MetadataMutation::Delete { key: key.to_owned() }
}

fn replica(meta: &MetaStore) -> Replica<'_> {
    Replica::new(meta, NonZeroUsize::new(100).unwrap())
}

#[tokio::test]
async fn test_sync_commits_metadata_journal_and_cursor_without_fetching_bytes() {
    let (dir, meta) = stores();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let digest = Digest::of(b"artifact");
    let size = b"artifact".len() as u64;
    let source = primary(vec![page(
        "primary-a",
        0,
        1,
        vec![change(
            1,
            vec![put("alpha\0upload", b"record"), delete("alpha\0stale")],
            vec![BlobReference {
                sha256: digest.as_str().to_owned(),
                size,
            }],
        )],
    )]);

    let (outcome, changed_keys, referenced) = replica(&meta).sync(&source).await.unwrap();

    assert_eq!(outcome.changes, 1);
    assert!(outcome.caught_up());
    assert_eq!(
        changed_keys,
        vec!["alpha\0stale".to_owned(), "alpha\0upload".to_owned()]
    );
    assert_eq!(referenced, vec![(digest.clone(), size)]);
    // Metadata (both the put and the delete), journal, and cursor are committed immediately.
    assert_eq!(
        meta.get_driver_value("alpha\0upload").unwrap().as_deref(),
        Some(b"record".as_slice())
    );
    assert!(meta.get_driver_value("alpha\0stale").unwrap().is_none());
    assert_eq!(meta.journal_after(0, 10).unwrap()[0].payload, b"event-1");
    assert_eq!(replica(&meta).state().unwrap().unwrap().serial, 1);
    // The referenced bytes are not present locally; the async blob plane pulls them later.
    assert!(blobs.head(&digest).await.unwrap().is_none());
}

#[tokio::test]
async fn test_sync_on_an_empty_page_commits_nothing() {
    let (_dir, meta) = stores();
    let source = primary_at(0, page("primary-a", 0, 0, vec![]));

    let (outcome, changed_keys, referenced) = replica(&meta).sync(&source).await.unwrap();

    assert_eq!(outcome.changes, 0);
    assert!(outcome.caught_up());
    assert!(changed_keys.is_empty());
    assert!(referenced.is_empty());
    assert!(replica(&meta).state().unwrap().is_none());
}

#[tokio::test]
async fn test_sync_resumes_from_the_committed_serial() {
    let (_dir, meta) = stores();
    let source = primary(vec![
        page("primary-a", 0, 2, vec![change(1, vec![put("key", b"one")], Vec::new())]),
        page("primary-a", 1, 2, vec![change(2, vec![put("key", b"two")], Vec::new())]),
    ]);
    let replica = replica(&meta);

    let (first, ..) = replica.sync(&source).await.unwrap();
    let (second, ..) = replica.sync(&source).await.unwrap();

    assert!(!first.caught_up());
    assert!(second.caught_up());
    assert_eq!(
        meta.get_driver_value("key").unwrap().as_deref(),
        Some(b"two".as_slice())
    );
    assert_eq!(*source.requests.lock().unwrap(), vec![0, 1]);
}

#[tokio::test]
async fn test_sync_rejects_a_serial_gap() {
    let (_dir, meta) = stores();
    let digest = Digest::of(b"artifact");
    let source = primary(vec![page(
        "primary-a",
        0,
        2,
        vec![change(
            2,
            Vec::new(),
            vec![BlobReference {
                sha256: digest.as_str().to_owned(),
                size: 8,
            }],
        )],
    )]);

    let result = replica(&meta).sync(&source).await;

    assert!(matches!(result, Err(SyncError::SerialGap { after: 0, actual: 2 })));
}

#[tokio::test]
async fn test_sync_rejects_a_different_source_after_progress() {
    let (_dir, meta) = stores();
    let first = primary(vec![page("primary-a", 0, 1, vec![change(1, Vec::new(), Vec::new())])]);
    replica(&meta).sync(&first).await.unwrap();
    let second = primary(vec![page("primary-b", 1, 1, Vec::new())]);

    let result = replica(&meta).sync(&second).await;

    assert!(matches!(result, Err(SyncError::SourceChanged { .. })));
    assert_eq!(replica(&meta).state().unwrap().unwrap().serial, 1);
}

#[tokio::test]
async fn test_sync_rejects_an_empty_page_while_the_primary_is_ahead() {
    let (_dir, meta) = stores();
    let source = primary(vec![page("primary-a", 0, 1, Vec::new())]);

    let result = replica(&meta).sync(&source).await;

    assert!(matches!(
        result,
        Err(SyncError::MissingChanges { after: 0, current: 1 })
    ));
}

#[tokio::test]
async fn test_sync_accepts_an_empty_page_at_the_primary_serial() {
    let (_dir, meta) = stores();
    let source = primary(vec![page("primary-a", 0, 0, Vec::new())]);

    let (outcome, changed_keys, referenced) = replica(&meta).sync(&source).await.unwrap();

    assert_eq!(outcome.changes, 0);
    assert_eq!(outcome.serial, 0);
    assert!(outcome.caught_up());
    assert!(changed_keys.is_empty());
    assert!(referenced.is_empty());
}

#[tokio::test]
async fn test_sync_rejects_an_unsupported_protocol_version() {
    let (_dir, meta) = stores();
    let mut invalid = page("primary-a", 0, 0, Vec::new());
    invalid.version = PROTOCOL_VERSION + 1;
    let source = primary(vec![invalid]);

    let result = replica(&meta).sync(&source).await;

    assert!(matches!(result, Err(SyncError::UnsupportedVersion { .. })));
}

#[tokio::test]
async fn test_sync_rejects_an_empty_source_identity() {
    let (_dir, meta) = stores();
    let source = primary(vec![page("", 0, 0, Vec::new())]);

    let result = replica(&meta).sync(&source).await;

    assert!(matches!(result, Err(SyncError::EmptySource)));
}

#[tokio::test]
async fn test_sync_rejects_a_page_for_another_cursor() {
    let (_dir, meta) = stores();
    let source = primary_at(0, page("primary-a", 1, 1, Vec::new()));

    let result = replica(&meta).sync(&source).await;

    assert!(matches!(
        result,
        Err(SyncError::WrongPageStart { expected: 0, actual: 1 })
    ));
}

#[tokio::test]
async fn test_sync_rejects_more_changes_than_requested() {
    let (_dir, meta) = stores();
    let source = primary(vec![page(
        "primary-a",
        0,
        2,
        vec![change(1, Vec::new(), Vec::new()), change(2, Vec::new(), Vec::new())],
    )]);
    let replica = Replica::new(&meta, NonZeroUsize::new(1).unwrap());

    let result = replica.sync(&source).await;

    assert!(matches!(result, Err(SyncError::PageTooLarge { limit: 1, actual: 2 })));
}

#[tokio::test]
async fn test_sync_rejects_a_reserved_metadata_key() {
    let (_dir, meta) = stores();
    let source = primary(vec![page(
        "primary-a",
        0,
        1,
        vec![change(1, vec![put("replication\0state", b"forged")], Vec::new())],
    )]);

    let result = replica(&meta).sync(&source).await;

    assert!(matches!(result, Err(SyncError::ReservedMetadataKey(_))));
}

#[tokio::test]
async fn test_sync_rejects_an_invalid_blob_digest() {
    let (_dir, meta) = stores();
    let source = primary(vec![page(
        "primary-a",
        0,
        1,
        vec![change(
            1,
            Vec::new(),
            vec![BlobReference {
                sha256: "invalid".to_owned(),
                size: 1,
            }],
        )],
    )]);

    let result = replica(&meta).sync(&source).await;

    assert!(matches!(result, Err(SyncError::InvalidDigest(_))));
}

#[tokio::test]
async fn test_sync_rejects_conflicting_sizes_for_one_blob() {
    let (_dir, meta) = stores();
    let digest = Digest::of(b"artifact");
    let source = primary(vec![page(
        "primary-a",
        0,
        1,
        vec![change(
            1,
            Vec::new(),
            vec![
                BlobReference {
                    sha256: digest.as_str().to_owned(),
                    size: 8,
                },
                BlobReference {
                    sha256: digest.as_str().to_owned(),
                    size: 9,
                },
            ],
        )],
    )]);

    let result = replica(&meta).sync(&source).await;

    assert!(matches!(result, Err(SyncError::ConflictingBlobSize { .. })));
}

#[tokio::test]
async fn test_sync_rejects_changes_ahead_of_the_primary_serial() {
    let (_dir, meta) = stores();
    let source = primary(vec![page("primary-a", 0, 0, vec![change(1, Vec::new(), Vec::new())])]);

    let result = replica(&meta).sync(&source).await;

    assert!(matches!(result, Err(SyncError::PrimaryBehind { current: 0, page: 1 })));
}

#[test]
fn test_state_rejects_a_local_journal_without_a_cursor() {
    let (_dir, meta) = stores();
    meta.next_serial().unwrap();

    let result = replica(&meta).state();

    assert!(matches!(
        result,
        Err(SyncError::LocalSerialMismatch { cursor: 0, journal: 1 })
    ));
}

#[tokio::test]
async fn test_sync_applies_the_last_metadata_mutation_in_a_page() {
    let (_dir, meta) = stores();
    meta.put_driver_value("key", b"old").unwrap();
    let source = primary(vec![page(
        "primary-a",
        0,
        2,
        vec![
            change(1, vec![put("key", b"new")], Vec::new()),
            change(2, vec![MetadataMutation::Delete { key: "key".to_owned() }], Vec::new()),
        ],
    )]);

    replica(&meta).sync(&source).await.unwrap();

    assert!(meta.get_driver_value("key").unwrap().is_none());
    assert_eq!(meta.current_serial().unwrap(), 2);
}

#[test]
fn test_protocol_encodes_opaque_bytes_as_base64() {
    let change = change(1, vec![put("key", &[0, 255])], Vec::new());
    let encoded = serde_json::to_value(&change).unwrap();

    assert_eq!(encoded["event"], "ZXZlbnQtMQ==");
    assert_eq!(encoded["metadata"][0]["value"], "AP8=");
    assert_eq!(serde_json::from_value::<Change>(encoded).unwrap(), change);
}
