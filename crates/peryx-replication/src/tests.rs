use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::sync::Mutex;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::stream;
use peryx_storage::blob::{BlobStorage, Digest};
use peryx_storage::meta::MetaStore;

use crate::{BlobReference, Change, ChangePage, MetadataMutation, PROTOCOL_VERSION, Primary, Replica, SyncError};

#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
struct PrimaryError(String);

struct TestPrimary {
    pages: BTreeMap<u64, ChangePage>,
    blobs: BTreeMap<String, Vec<Result<Bytes, PrimaryError>>>,
    requests: Mutex<Vec<u64>>,
}

#[async_trait]
impl Primary for TestPrimary {
    type Error = PrimaryError;
    type BlobStream = stream::Iter<std::vec::IntoIter<Result<Bytes, PrimaryError>>>;

    async fn changes(&self, after: u64, _limit: usize) -> Result<ChangePage, Self::Error> {
        self.requests.lock().unwrap().push(after);
        self.pages
            .get(&after)
            .cloned()
            .ok_or_else(|| PrimaryError(format!("no page after {after}")))
    }

    async fn blob(&self, digest: &Digest) -> Result<Self::BlobStream, Self::Error> {
        self.blobs
            .get(digest.as_str())
            .cloned()
            .map(stream::iter)
            .ok_or_else(|| PrimaryError(format!("no blob {}", digest.as_str())))
    }
}

fn stores() -> (tempfile::TempDir, MetaStore, BlobStorage) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    (dir, meta, blobs)
}

fn primary(pages: Vec<ChangePage>, blobs: Vec<(&Digest, Vec<Result<Bytes, PrimaryError>>)>) -> TestPrimary {
    TestPrimary {
        pages: pages.into_iter().map(|page| (page.after, page)).collect(),
        blobs: blobs
            .into_iter()
            .map(|(digest, chunks)| (digest.as_str().to_owned(), chunks))
            .collect(),
        requests: Mutex::default(),
    }
}

fn primary_at(after: u64, page: ChangePage) -> TestPrimary {
    TestPrimary {
        pages: BTreeMap::from([(after, page)]),
        blobs: BTreeMap::new(),
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

fn replica<'store>(meta: &'store MetaStore, blobs: &'store BlobStorage) -> Replica<'store> {
    Replica::new(meta, blobs, NonZeroUsize::new(100).unwrap())
}

#[tokio::test]
async fn test_sync_commits_verified_blob_metadata_journal_and_cursor() {
    let (_dir, meta, blobs) = stores();
    let bytes = Bytes::from_static(b"artifact");
    let digest = Digest::of(&bytes);
    let source = primary(
        vec![page(
            "primary-a",
            0,
            1,
            vec![change(
                1,
                vec![put("pypi\0upload", b"record")],
                vec![BlobReference {
                    sha256: digest.as_str().to_owned(),
                    size: bytes.len() as u64,
                }],
            )],
        )],
        vec![(&digest, vec![Ok(bytes.clone())])],
    );

    let outcome = replica(&meta, &blobs).sync_once(&source).await.unwrap();

    assert_eq!(outcome.changes, 1);
    assert_eq!(outcome.blobs, 1);
    assert!(outcome.caught_up());
    assert_eq!(blobs.read_bytes(&digest, bytes.len() as u64).await.unwrap(), bytes);
    assert_eq!(
        meta.get_driver_value("pypi\0upload").unwrap().as_deref(),
        Some(b"record".as_slice())
    );
    let journal = meta.journal_after(0, 10).unwrap();
    assert_eq!(journal[0].payload, b"event-1");
    assert_eq!(
        journal[0].blobs,
        vec![peryx_storage::meta::DriverBlobReference {
            sha256: digest.as_str().to_owned(),
            size: bytes.len() as u64,
        }]
    );
    assert_eq!(replica(&meta, &blobs).state().unwrap().unwrap().serial, 1);
}

#[tokio::test]
async fn test_sync_resumes_from_the_committed_serial() {
    let (_dir, meta, blobs) = stores();
    let source = primary(
        vec![
            page("primary-a", 0, 2, vec![change(1, vec![put("key", b"one")], Vec::new())]),
            page("primary-a", 1, 2, vec![change(2, vec![put("key", b"two")], Vec::new())]),
        ],
        Vec::new(),
    );
    let replica = replica(&meta, &blobs);

    let first = replica.sync_once(&source).await.unwrap();
    let second = replica.sync_once(&source).await.unwrap();

    assert!(!first.caught_up());
    assert!(second.caught_up());
    assert_eq!(
        meta.get_driver_value("key").unwrap().as_deref(),
        Some(b"two".as_slice())
    );
    assert_eq!(*source.requests.lock().unwrap(), vec![0, 1]);
}

#[tokio::test]
async fn test_sync_digest_mismatch_keeps_prior_cursor_and_metadata() {
    let (_dir, meta, blobs) = stores();
    let expected = Digest::of(b"correct");
    let source = primary(
        vec![page(
            "primary-a",
            0,
            1,
            vec![change(
                1,
                vec![put("key", b"value")],
                vec![BlobReference {
                    sha256: expected.as_str().to_owned(),
                    size: 7,
                }],
            )],
        )],
        vec![(&expected, vec![Ok(Bytes::from_static(b"badness"))])],
    );

    let result = replica(&meta, &blobs).sync_once(&source).await;

    assert!(matches!(result, Err(SyncError::Blob(_))));
    assert!(meta.get_driver_value("key").unwrap().is_none());
    assert!(replica(&meta, &blobs).state().unwrap().is_none());
    assert_eq!(meta.current_serial().unwrap(), 0);
    assert!(blobs.head(&expected).await.unwrap().is_none());
}

#[tokio::test]
async fn test_sync_interrupted_blob_keeps_prior_cursor_and_metadata() {
    let (_dir, meta, blobs) = stores();
    let digest = Digest::of(b"complete");
    let source = primary(
        vec![page(
            "primary-a",
            0,
            1,
            vec![change(
                1,
                vec![put("key", b"value")],
                vec![BlobReference {
                    sha256: digest.as_str().to_owned(),
                    size: 8,
                }],
            )],
        )],
        vec![(
            &digest,
            vec![
                Ok(Bytes::from_static(b"part")),
                Err(PrimaryError("connection lost".to_owned())),
            ],
        )],
    );

    let result = replica(&meta, &blobs).sync_once(&source).await;

    assert!(matches!(result, Err(SyncError::Primary(_))));
    assert!(meta.get_driver_value("key").unwrap().is_none());
    assert!(replica(&meta, &blobs).state().unwrap().is_none());
    assert!(blobs.head(&digest).await.unwrap().is_none());
}

#[tokio::test]
async fn test_sync_primary_blob_open_failure_removes_the_stage() {
    let (_dir, meta, blobs) = stores();
    let digest = Digest::of(b"missing");
    let source = primary(
        vec![page(
            "primary-a",
            0,
            1,
            vec![change(
                1,
                vec![put("key", b"value")],
                vec![BlobReference {
                    sha256: digest.as_str().to_owned(),
                    size: 7,
                }],
            )],
        )],
        Vec::new(),
    );

    let result = replica(&meta, &blobs).sync_once(&source).await;

    assert!(matches!(result, Err(SyncError::Primary(_))));
    assert!(blobs.head(&digest).await.unwrap().is_none());
}

#[tokio::test]
async fn test_sync_rejects_a_serial_gap_before_fetching_blobs() {
    let (_dir, meta, blobs) = stores();
    let digest = Digest::of(b"artifact");
    let source = primary(
        vec![page(
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
        )],
        Vec::new(),
    );

    let result = replica(&meta, &blobs).sync_once(&source).await;

    assert!(matches!(result, Err(SyncError::SerialGap { after: 0, actual: 2 })));
}

#[tokio::test]
async fn test_sync_rejects_a_different_source_after_progress() {
    let (_dir, meta, blobs) = stores();
    let first = primary(
        vec![page("primary-a", 0, 1, vec![change(1, Vec::new(), Vec::new())])],
        Vec::new(),
    );
    replica(&meta, &blobs).sync_once(&first).await.unwrap();
    let second = primary(vec![page("primary-b", 1, 1, Vec::new())], Vec::new());

    let result = replica(&meta, &blobs).sync_once(&second).await;

    assert!(matches!(result, Err(SyncError::SourceChanged { .. })));
    assert_eq!(replica(&meta, &blobs).state().unwrap().unwrap().serial, 1);
}

#[tokio::test]
async fn test_sync_rejects_an_empty_page_while_the_primary_is_ahead() {
    let (_dir, meta, blobs) = stores();
    let source = primary(vec![page("primary-a", 0, 1, Vec::new())], Vec::new());

    let result = replica(&meta, &blobs).sync_once(&source).await;

    assert!(matches!(
        result,
        Err(SyncError::MissingChanges { after: 0, current: 1 })
    ));
}

#[tokio::test]
async fn test_sync_accepts_an_empty_page_at_the_primary_serial() {
    let (_dir, meta, blobs) = stores();
    let source = primary(vec![page("primary-a", 0, 0, Vec::new())], Vec::new());

    let outcome = replica(&meta, &blobs).sync_once(&source).await.unwrap();

    assert_eq!(outcome.changes, 0);
    assert_eq!(outcome.serial, 0);
    assert!(outcome.caught_up());
}

#[tokio::test]
async fn test_sync_rejects_an_unsupported_protocol_version() {
    let (_dir, meta, blobs) = stores();
    let mut invalid = page("primary-a", 0, 0, Vec::new());
    invalid.version = PROTOCOL_VERSION + 1;
    let source = primary(vec![invalid], Vec::new());

    let result = replica(&meta, &blobs).sync_once(&source).await;

    assert!(matches!(result, Err(SyncError::UnsupportedVersion { .. })));
}

#[tokio::test]
async fn test_sync_rejects_an_empty_source_identity() {
    let (_dir, meta, blobs) = stores();
    let source = primary(vec![page("", 0, 0, Vec::new())], Vec::new());

    let result = replica(&meta, &blobs).sync_once(&source).await;

    assert!(matches!(result, Err(SyncError::EmptySource)));
}

#[tokio::test]
async fn test_sync_rejects_a_page_for_another_cursor() {
    let (_dir, meta, blobs) = stores();
    let source = primary_at(0, page("primary-a", 1, 1, Vec::new()));

    let result = replica(&meta, &blobs).sync_once(&source).await;

    assert!(matches!(
        result,
        Err(SyncError::WrongPageStart { expected: 0, actual: 1 })
    ));
}

#[tokio::test]
async fn test_sync_rejects_more_changes_than_requested() {
    let (_dir, meta, blobs) = stores();
    let source = primary(
        vec![page(
            "primary-a",
            0,
            2,
            vec![change(1, Vec::new(), Vec::new()), change(2, Vec::new(), Vec::new())],
        )],
        Vec::new(),
    );
    let replica = Replica::new(&meta, &blobs, NonZeroUsize::new(1).unwrap());

    let result = replica.sync_once(&source).await;

    assert!(matches!(result, Err(SyncError::PageTooLarge { limit: 1, actual: 2 })));
}

#[tokio::test]
async fn test_sync_rejects_a_reserved_metadata_key() {
    let (_dir, meta, blobs) = stores();
    let source = primary(
        vec![page(
            "primary-a",
            0,
            1,
            vec![change(1, vec![put("replication\0state", b"forged")], Vec::new())],
        )],
        Vec::new(),
    );

    let result = replica(&meta, &blobs).sync_once(&source).await;

    assert!(matches!(result, Err(SyncError::ReservedMetadataKey(_))));
}

#[tokio::test]
async fn test_sync_rejects_an_invalid_blob_digest() {
    let (_dir, meta, blobs) = stores();
    let source = primary(
        vec![page(
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
        )],
        Vec::new(),
    );

    let result = replica(&meta, &blobs).sync_once(&source).await;

    assert!(matches!(result, Err(SyncError::InvalidDigest(_))));
}

#[tokio::test]
async fn test_sync_rejects_conflicting_sizes_for_one_blob() {
    let (_dir, meta, blobs) = stores();
    let digest = Digest::of(b"artifact");
    let source = primary(
        vec![page(
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
        )],
        Vec::new(),
    );

    let result = replica(&meta, &blobs).sync_once(&source).await;

    assert!(matches!(result, Err(SyncError::ConflictingBlobSize { .. })));
}

#[tokio::test]
async fn test_sync_rejects_changes_ahead_of_the_primary_serial() {
    let (_dir, meta, blobs) = stores();
    let source = primary(
        vec![page("primary-a", 0, 0, vec![change(1, Vec::new(), Vec::new())])],
        Vec::new(),
    );

    let result = replica(&meta, &blobs).sync_once(&source).await;

    assert!(matches!(result, Err(SyncError::PrimaryBehind { current: 0, page: 1 })));
}

#[tokio::test]
async fn test_sync_rejects_a_corrupt_existing_blob() {
    let (_dir, meta, blobs) = stores();
    let digest = blobs.put_bytes(b"artifact").await.unwrap();
    // Rewrite the stored bytes in place to the same length, so the size check passes and verification
    // is what rejects the blob. Writing through the backend's own entry path keeps this cross-platform:
    // a materialized lease holds a shared file lock that Windows enforces against writers.
    blobs
        .blocking()
        .visit(|entry| std::fs::write(&entry.path, b"corrupt!"))
        .unwrap();
    let source = primary(
        vec![page(
            "primary-a",
            0,
            1,
            vec![change(
                1,
                Vec::new(),
                vec![BlobReference {
                    sha256: digest.as_str().to_owned(),
                    size: 8,
                }],
            )],
        )],
        Vec::new(),
    );

    let result = replica(&meta, &blobs).sync_once(&source).await;

    assert!(matches!(result, Err(SyncError::CorruptBlob(_))));
    assert_eq!(meta.current_serial().unwrap(), 0);
}

#[test]
fn test_state_rejects_a_local_journal_without_a_cursor() {
    let (_dir, meta, blobs) = stores();
    meta.next_serial().unwrap();

    let result = replica(&meta, &blobs).state();

    assert!(matches!(
        result,
        Err(SyncError::LocalSerialMismatch { cursor: 0, journal: 1 })
    ));
}

#[tokio::test]
async fn test_sync_size_mismatch_keeps_prior_cursor_and_metadata() {
    let (_dir, meta, blobs) = stores();
    let bytes = Bytes::from_static(b"artifact");
    let digest = Digest::of(&bytes);
    let source = primary(
        vec![page(
            "primary-a",
            0,
            1,
            vec![change(
                1,
                vec![put("key", b"value")],
                vec![BlobReference {
                    sha256: digest.as_str().to_owned(),
                    size: 7,
                }],
            )],
        )],
        vec![(&digest, vec![Ok(bytes)])],
    );

    let result = replica(&meta, &blobs).sync_once(&source).await;

    assert!(matches!(
        result,
        Err(SyncError::BlobSizeMismatch {
            expected: 7,
            actual: 8,
            ..
        })
    ));
    assert!(meta.get_driver_value("key").unwrap().is_none());
    assert!(replica(&meta, &blobs).state().unwrap().is_none());
    assert!(blobs.head(&digest).await.unwrap().is_none());
}

#[tokio::test]
async fn test_sync_short_blob_keeps_prior_cursor_and_metadata() {
    let (_dir, meta, blobs) = stores();
    let bytes = Bytes::from_static(b"artifact");
    let digest = Digest::of(&bytes);
    let source = primary(
        vec![page(
            "primary-a",
            0,
            1,
            vec![change(
                1,
                vec![put("key", b"value")],
                vec![BlobReference {
                    sha256: digest.as_str().to_owned(),
                    size: 9,
                }],
            )],
        )],
        vec![(&digest, vec![Ok(bytes)])],
    );

    let result = replica(&meta, &blobs).sync_once(&source).await;

    assert!(matches!(
        result,
        Err(SyncError::BlobSizeMismatch {
            expected: 9,
            actual: 8,
            ..
        })
    ));
    assert!(meta.get_driver_value("key").unwrap().is_none());
    assert!(blobs.head(&digest).await.unwrap().is_none());
}

#[tokio::test]
async fn test_sync_applies_the_last_metadata_mutation_in_a_page() {
    let (_dir, meta, blobs) = stores();
    meta.put_driver_value("key", b"old").unwrap();
    let source = primary(
        vec![page(
            "primary-a",
            0,
            2,
            vec![
                change(1, vec![put("key", b"new")], Vec::new()),
                change(2, vec![MetadataMutation::Delete { key: "key".to_owned() }], Vec::new()),
            ],
        )],
        Vec::new(),
    );

    replica(&meta, &blobs).sync_once(&source).await.unwrap();

    assert!(meta.get_driver_value("key").unwrap().is_none());
    assert_eq!(meta.current_serial().unwrap(), 2);
}

#[tokio::test]
async fn test_sync_reuses_an_existing_verified_blob() {
    let (_dir, meta, blobs) = stores();
    let digest = blobs.put_bytes(b"artifact").await.unwrap();
    let source = primary(
        vec![page(
            "primary-a",
            0,
            1,
            vec![change(
                1,
                Vec::new(),
                vec![BlobReference {
                    sha256: digest.as_str().to_owned(),
                    size: 8,
                }],
            )],
        )],
        Vec::new(),
    );

    let outcome = replica(&meta, &blobs).sync_once(&source).await.unwrap();

    assert_eq!(outcome.blobs, 0);
    assert_eq!(blobs.read_bytes(&digest, 8).await.unwrap(), b"artifact");
}

#[tokio::test]
async fn test_sync_rejects_the_wrong_size_for_an_existing_blob() {
    let (_dir, meta, blobs) = stores();
    let digest = blobs.put_bytes(b"artifact").await.unwrap();
    let source = primary(
        vec![page(
            "primary-a",
            0,
            1,
            vec![change(
                1,
                Vec::new(),
                vec![BlobReference {
                    sha256: digest.as_str().to_owned(),
                    size: 9,
                }],
            )],
        )],
        Vec::new(),
    );

    let error = replica(&meta, &blobs).sync_once(&source).await.unwrap_err();

    assert!(matches!(
        error,
        SyncError::BlobSizeMismatch {
            expected: 9,
            actual: 8,
            ..
        }
    ));
}

#[test]
fn test_protocol_encodes_opaque_bytes_as_base64() {
    let change = change(1, vec![put("key", &[0, 255])], Vec::new());
    let encoded = serde_json::to_value(&change).unwrap();

    assert_eq!(encoded["event"], "ZXZlbnQtMQ==");
    assert_eq!(encoded["metadata"][0]["value"], "AP8=");
    assert_eq!(serde_json::from_value::<Change>(encoded).unwrap(), change);
}
