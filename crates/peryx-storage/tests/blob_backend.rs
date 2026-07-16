use peryx_storage::blob::{BlobBackend, BlobError, BlobMetadata, BlobOperation, BlobStore, Digest};

fn store() -> (tempfile::TempDir, BlobStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path());
    (dir, store)
}

async fn exercise(store: &impl BlobBackend) {
    let digest = Digest::of(b"package");
    assert_eq!(store.head(digest.clone()).await.unwrap(), None);
    store.put(digest.clone(), b"package".to_vec()).await.unwrap();
    assert_eq!(
        store.head(digest.clone()).await.unwrap(),
        Some(BlobMetadata { bytes: 7 })
    );
    assert_eq!(store.get(digest.clone()).await.unwrap(), b"package");
    assert_eq!(store.range(digest.clone(), 1..5).await.unwrap(), b"acka");
    assert!(store.verify(digest.clone()).await.unwrap());
    assert!(store.delete(digest.clone()).await.unwrap());
    assert!(!store.delete(digest.clone()).await.unwrap());
    assert_eq!(store.head(digest).await.unwrap(), None);
}

#[tokio::test]
async fn test_blob_backend_round_trips() {
    let (_dir, store) = store();
    exercise(&store).await;
}

#[test]
fn test_staged_blob_reports_digest_and_length() {
    let (_dir, store) = store();
    let mut pending = store.begin().unwrap();
    pending.write(b"staged").unwrap();
    let staged = pending.finish().unwrap();
    assert_eq!(
        (staged.digest(), staged.len(), staged.is_empty()),
        (&Digest::of(b"staged"), 6, false)
    );
    store.commit_staged(staged).unwrap();
    assert_eq!(store.read(&Digest::of(b"staged")).unwrap(), b"staged");
}

#[tokio::test]
async fn test_blob_backend_rejects_a_digest_mismatch_with_context() {
    let (_dir, store) = store();
    let digest = Digest::of(b"expected");
    let err = store.put(digest.clone(), b"other".to_vec()).await.unwrap_err();
    assert!(matches!(
        err,
        BlobError::Backend {
            backend: "filesystem",
            operation: BlobOperation::Put,
            ref digest,
            ..
        } if digest == Digest::of(b"expected").as_str()
    ));
}

#[tokio::test]
async fn test_blob_backend_rejects_an_invalid_range_with_context() {
    let (_dir, store) = store();
    let digest = Digest::of(b"package");
    store.put(digest.clone(), b"package".to_vec()).await.unwrap();
    let err = store.range(digest.clone(), 3..9).await.unwrap_err();
    assert!(matches!(
        err,
        BlobError::Backend {
            backend: "filesystem",
            operation: BlobOperation::Range,
            ref digest,
            ..
        } if digest == Digest::of(b"package").as_str()
    ));
}

#[tokio::test]
async fn test_blob_backend_range_names_the_missing_digest() {
    let (_dir, store) = store();
    let digest = Digest::of(b"missing");
    let err = store.range(digest.clone(), 0..1).await.unwrap_err();
    assert!(matches!(
        err,
        BlobError::Backend {
            backend: "filesystem",
            operation: BlobOperation::Range,
            ref digest,
            ..
        } if digest == Digest::of(b"missing").as_str()
    ));
}

#[tokio::test]
async fn test_blob_backend_get_names_the_missing_digest() {
    let (_dir, store) = store();
    let digest = Digest::of(b"missing");
    let err = store.get(digest.clone()).await.unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "filesystem blob backend get failed for {}: blob {} not found",
            digest.as_str(),
            digest.as_str()
        )
    );
}
