use std::io::{Read as _, Seek as _, SeekFrom};

use futures_util::{StreamExt as _, TryStreamExt as _};
use peryx_storage::blob::{
    BlobBackend, BlobDurability, BlobError, BlobErrorKind, BlobOperation, BlobRead, BlobReadBody, BlobStorage,
    BlobSupport, Digest,
};

async fn bytes(read: BlobRead) -> Vec<u8> {
    match read.body {
        BlobReadBody::File(mut file) => {
            file.seek(SeekFrom::Start(read.range.start)).unwrap();
            let mut bytes = vec![0; usize::try_from(read.range.end - read.range.start).unwrap()];
            file.read_exact(&mut bytes).unwrap();
            bytes
        }
        BlobReadBody::Stream(stream) => stream
            .try_fold(Vec::new(), |mut bytes, chunk| async move {
                bytes.extend_from_slice(&chunk);
                Ok(bytes)
            })
            .await
            .unwrap(),
    }
}

async fn exercise(backend: &impl BlobBackend) {
    backend.health().await.unwrap();
    let digest = Digest::of(b"package");
    assert!(backend.head(digest.clone()).await.unwrap().is_none());
    let mut write = backend.begin().await.unwrap();
    write.write_chunk(bytes::Bytes::from_static(b"pack")).await.unwrap();
    write.write_chunk(bytes::Bytes::from_static(b"age")).await.unwrap();
    write.commit(&digest).await.unwrap();
    assert_eq!(backend.head(digest.clone()).await.unwrap().unwrap().bytes, 7);
    assert_eq!(
        bytes(backend.open(digest.clone(), None).await.unwrap()).await,
        b"package"
    );
    assert_eq!(
        bytes(backend.open(digest.clone(), Some(1..5)).await.unwrap()).await,
        b"acka"
    );
    assert!(backend.verify(digest.clone()).await.unwrap());
    assert!(backend.materialize(digest.clone()).await.unwrap().path().is_file());
    assert!(backend.delete(digest.clone()).await.unwrap());
    assert!(!backend.delete(digest.clone()).await.unwrap());
}

#[tokio::test]
async fn test_filesystem_and_runtime_facade_share_the_backend_contract() {
    let first = tempfile::tempdir().unwrap();
    exercise(&peryx_storage::blob::BlobStore::new(first.path())).await;
    let second = tempfile::tempdir().unwrap();
    exercise(&BlobStorage::filesystem(second.path())).await;
}

#[tokio::test]
async fn test_facade_health_and_bulk_presence() {
    let dir = tempfile::tempdir().unwrap();
    let storage = BlobStorage::filesystem(dir.path());
    storage.health().await.unwrap();
    let first = storage.put_bytes(b"first").await.unwrap();
    let second = storage.put_bytes(b"second").await.unwrap();
    let missing = Digest::of(b"missing");
    assert_eq!(
        storage
            .present(vec![first.clone(), missing, second.clone(), first.clone()])
            .await
            .unwrap(),
        std::collections::HashSet::from([first, second])
    );
    assert!(storage.present(Vec::new()).await.unwrap().is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn test_bulk_presence_reports_a_filesystem_error() {
    let dir = tempfile::tempdir().unwrap();
    let storage = BlobStorage::filesystem(dir.path());
    let digest = Digest::of(b"loop");
    let hex = digest.as_str();
    let path = dir.path().join(format!("sha256/{}/{}/{}", &hex[..2], &hex[2..4], hex));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&path, &path).unwrap();
    let error = storage.present(vec![digest]).await.unwrap_err();
    assert_eq!(error.kind(), BlobErrorKind::Io);
    assert_eq!(error.context().unwrap().operation, BlobOperation::Head);
}

#[test]
fn test_filesystem_capabilities_are_actionable() {
    fn capabilities(backend: &impl BlobBackend) -> peryx_storage::blob::BlobCapabilities {
        backend.capabilities()
    }

    let dir = tempfile::tempdir().unwrap();
    let storage = BlobStorage::filesystem(dir.path());
    let capabilities = capabilities(&storage);
    assert_eq!(storage.name(), "filesystem");
    assert_eq!(capabilities.durability, BlobDurability::Filesystem);
    assert_eq!(capabilities.create_if_absent, BlobSupport::Native);
    assert_eq!(capabilities.range, BlobSupport::Native);
    assert_eq!(capabilities.checksum, BlobSupport::Emulated);
    assert_eq!(capabilities.delete, BlobSupport::Native);
    assert_eq!(capabilities.list, BlobSupport::Native);
    assert_eq!(capabilities.local_tail, BlobSupport::Native);
    assert_eq!(BlobDurability::Filesystem.as_str(), "filesystem");
    assert_eq!(BlobSupport::Native.as_str(), "native");
    assert_eq!(BlobSupport::Emulated.as_str(), "emulated");
    assert_eq!(BlobSupport::Unsupported.as_str(), "unsupported");
}

#[tokio::test]
async fn test_streamed_write_exposes_an_opaque_tail_and_stage() {
    let dir = tempfile::tempdir().unwrap();
    let storage = BlobStorage::filesystem(dir.path());
    let mut write = storage.begin().await.unwrap();
    let tail = write.tail().unwrap();
    write.write_chunk(bytes::Bytes::from_static(b"staged")).await.unwrap();
    assert_eq!(write.flush().await.unwrap(), 6);
    let mut visible = String::new();
    tail.open().unwrap().read_to_string(&mut visible).unwrap();
    assert_eq!(visible, "staged");
    let staged = write.finish().await.unwrap();
    assert_eq!(
        (staged.digest(), staged.len(), staged.is_empty()),
        (&Digest::of(b"staged"), 6, false)
    );
    assert_eq!(staged.with_materialized(std::path::Path::metadata).unwrap().len(), 6);
    staged.commit().await.unwrap();
    assert_eq!(storage.read_bytes(&Digest::of(b"staged"), 6).await.unwrap(), b"staged");
}

#[tokio::test]
async fn test_staged_commit_reports_a_destination_error() {
    let dir = tempfile::tempdir().unwrap();
    let storage = BlobStorage::filesystem(dir.path());
    let staged = storage.stage_bytes(b"package").await.unwrap();
    std::fs::write(dir.path().join("sha256"), b"not a directory").unwrap();
    let error = staged.commit().await.unwrap_err();
    assert_eq!(error.kind(), BlobErrorKind::Io);
    assert_eq!(error.context().unwrap().operation, BlobOperation::Commit);
}

#[tokio::test]
async fn test_streamed_write_preserves_order_across_automatic_batches() {
    let dir = tempfile::tempdir().unwrap();
    let storage = BlobStorage::filesystem(dir.path());
    let mut expected = vec![1u8; 1024 * 1024];
    expected.extend_from_slice(b"between");
    expected.extend(std::iter::repeat_n(2u8, 1024 * 1024));
    let mut write = storage.begin().await.unwrap();
    write
        .write_chunk(bytes::Bytes::from(vec![1u8; 1024 * 1024]))
        .await
        .unwrap();
    write.write_chunk(bytes::Bytes::from_static(b"between")).await.unwrap();
    write
        .write_chunk(bytes::Bytes::from(vec![2u8; 1024 * 1024]))
        .await
        .unwrap();
    write.commit(&Digest::of(&expected)).await.unwrap();
    assert_eq!(
        storage
            .read_bytes(&Digest::of(&expected), expected.len() as u64)
            .await
            .unwrap(),
        expected
    );
}

#[tokio::test]
async fn test_streamed_write_flushes_monotonic_tail_lengths() {
    let dir = tempfile::tempdir().unwrap();
    let storage = BlobStorage::filesystem(dir.path());
    let mut write = storage.begin().await.unwrap();
    let mut tail = write.tail().unwrap().open().unwrap();
    write.write_chunk(bytes::Bytes::from_static(b"first")).await.unwrap();
    assert_eq!(write.flush().await.unwrap(), 5);
    let mut first = [0; 5];
    tail.read_exact(&mut first).unwrap();
    assert_eq!(&first, b"first");
    write.write_chunk(bytes::Bytes::new()).await.unwrap();
    write.write_chunk(bytes::Bytes::from_static(b"second")).await.unwrap();
    assert_eq!(write.flush().await.unwrap(), 11);
    let mut second = [0; 6];
    tail.read_exact(&mut second).unwrap();
    assert_eq!(&second, b"second");
    write.abort().await.unwrap();
}

#[tokio::test]
async fn test_streamed_write_abort_removes_the_stage() {
    let dir = tempfile::tempdir().unwrap();
    let storage = BlobStorage::filesystem(dir.path());
    let mut write = storage.begin().await.unwrap();
    let tail = write.tail().unwrap();
    write
        .write_chunk(bytes::Bytes::from_static(b"discarded"))
        .await
        .unwrap();
    write.abort().await.unwrap();
    assert_eq!(tail.open().unwrap_err().kind(), std::io::ErrorKind::NotFound);
}

#[tokio::test]
async fn test_streamed_write_drop_waits_for_an_accepted_batch() {
    let dir = tempfile::tempdir().unwrap();
    let mut write = BlobStorage::filesystem(dir.path()).begin().await.unwrap();
    let tail = write.tail().unwrap();
    write
        .write_chunk(bytes::Bytes::from(vec![0; 1024 * 1024]))
        .await
        .unwrap();
    drop(write);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match tail.open() {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Ok(_) => tokio::task::yield_now().await,
                Err(error) => panic!("staged blob cleanup failed: {error}"),
            }
        }
    })
    .await
    .unwrap();
}

#[test]
fn test_streamed_write_drop_outside_runtime_removes_the_stage() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
    let (write, tail) = runtime.block_on(async {
        let write = BlobStorage::filesystem(dir.path()).begin().await.unwrap();
        let tail = write.tail().unwrap();
        (write, tail)
    });
    drop(runtime);
    drop(write);
    assert_eq!(tail.open().unwrap_err().kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn test_staged_blob_drop_outside_runtime_removes_the_stage() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
    let staged = runtime
        .block_on(BlobStorage::filesystem(dir.path()).stage_bytes(b"discarded"))
        .unwrap();
    let path = staged.with_materialized(std::path::Path::to_owned);
    drop(runtime);
    drop(staged);
    assert!(!path.exists());
}

#[tokio::test]
async fn test_materialized_lease_pins_bytes_until_drop() {
    let dir = tempfile::tempdir().unwrap();
    let storage = BlobStorage::filesystem(dir.path());
    let digest = storage.put_bytes(b"package").await.unwrap();
    let lease = storage.materialize(&digest).await.unwrap();
    let leased_path = lease.path().to_owned();
    assert!(storage.delete(&digest).await.unwrap());
    assert_eq!(std::fs::read(&leased_path).unwrap(), b"package");
    drop(lease);
    assert!(!leased_path.exists());
}

#[tokio::test]
async fn test_health_removes_only_inactive_materialized_leases() {
    let dir = tempfile::tempdir().unwrap();
    let storage = BlobStorage::filesystem(dir.path());
    let digest = storage.put_bytes(b"package").await.unwrap();
    let lease = storage.materialize(&digest).await.unwrap();
    let active = lease.path().to_owned();
    let stale = dir.path().join(".leases/.peryx-lease-stale");
    std::fs::write(&stale, b"stale").unwrap();
    storage.health().await.unwrap();
    assert!(active.is_file());
    assert!(!stale.exists());
    drop(lease);
}

#[test]
fn test_blocking_adapter_supports_offline_maintenance() {
    let dir = tempfile::tempdir().unwrap();
    let storage = BlobStorage::filesystem(dir.path());
    let blocking = storage.blocking();
    let digest = blocking.put_bytes(b"package").unwrap();
    assert_eq!(blocking.read_bytes(&digest, 7).unwrap(), b"package");
    assert_eq!(blocking.head(&digest).unwrap().unwrap().bytes, 7);
    assert!(blocking.verify(&digest).unwrap());
    assert!(blocking.materialize(&digest).unwrap().path().is_file());
    let mut seen = Vec::new();
    blocking
        .visit(|entry| {
            seen.push((entry.digest, entry.bytes));
            Ok::<_, std::convert::Infallible>(())
        })
        .unwrap();
    assert_eq!(seen, vec![(Some(digest.clone()), 7)]);
    assert!(blocking.delete(&digest).unwrap());
}

#[test]
fn test_blocking_read_reports_a_missing_blob() {
    let dir = tempfile::tempdir().unwrap();
    let storage = BlobStorage::filesystem(dir.path());
    assert_eq!(
        storage
            .blocking()
            .read_bytes(&Digest::of(b"missing"), 7)
            .unwrap_err()
            .kind(),
        BlobErrorKind::NotFound
    );
}

#[test]
fn test_blocking_read_enforces_the_size_limit() {
    let dir = tempfile::tempdir().unwrap();
    let storage = BlobStorage::filesystem(dir.path());
    let digest = storage.blocking().put_bytes(b"package").unwrap();
    assert_eq!(
        storage.blocking().read_bytes(&digest, 6).unwrap_err().kind(),
        BlobErrorKind::LimitExceeded
    );
}

#[test]
fn test_blocking_adapter_rejects_a_digest_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let storage = BlobStorage::filesystem(dir.path());
    let expected = Digest::of(b"expected");
    assert_eq!(
        storage
            .blocking()
            .put_bytes_as(b"actual", &expected)
            .unwrap_err()
            .kind(),
        BlobErrorKind::DigestMismatch
    );
}

#[cfg(unix)]
#[test]
fn test_blocking_read_reports_a_filesystem_error() {
    let dir = tempfile::tempdir().unwrap();
    let storage = BlobStorage::filesystem(dir.path());
    let digest = Digest::of(b"loop");
    let hex = digest.as_str();
    let path = dir.path().join(format!("sha256/{}/{}/{}", &hex[..2], &hex[2..4], hex));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&path, &path).unwrap();
    assert_eq!(
        storage.blocking().read_bytes(&digest, 7).unwrap_err().kind(),
        BlobErrorKind::Io
    );
}

#[test]
fn test_blocking_materialize_reports_a_lease_directory_error() {
    let dir = tempfile::tempdir().unwrap();
    let storage = BlobStorage::filesystem(dir.path());
    let digest = storage.blocking().put_bytes(b"package").unwrap();
    std::fs::write(dir.path().join(".leases"), b"not a directory").unwrap();
    assert_eq!(
        storage.blocking().materialize(&digest).unwrap_err().kind(),
        BlobErrorKind::Io
    );
}

#[tokio::test]
async fn test_bounded_reads_reject_large_blobs() {
    let dir = tempfile::tempdir().unwrap();
    let storage = BlobStorage::filesystem(dir.path());
    let digest = storage.put_bytes(b"package").await.unwrap();
    let err = storage.read_bytes(&digest, 6).await.unwrap_err();
    assert_eq!(err.kind(), BlobErrorKind::LimitExceeded);
    assert_eq!(err.context().unwrap().operation, BlobOperation::Open);
}

#[tokio::test]
async fn test_collected_file_read_honors_range() {
    let dir = tempfile::tempdir().unwrap();
    let storage = BlobStorage::filesystem(dir.path());
    let digest = storage.put_bytes(b"package").await.unwrap();
    assert_eq!(
        storage
            .open(&digest, Some(1..5))
            .await
            .unwrap()
            .collect(4)
            .await
            .unwrap(),
        b"acka"
    );
    assert!(
        storage
            .open(&digest, Some(3..3))
            .await
            .unwrap()
            .collect(0)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn test_collected_file_read_reports_truncation_with_context() {
    let dir = tempfile::tempdir().unwrap();
    let storage = BlobStorage::filesystem(dir.path());
    let digest = storage.put_bytes(b"package").await.unwrap();
    let read = storage.open(&digest, None).await.unwrap();
    let lease = storage.materialize(&digest).await.unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(lease.path())
        .unwrap()
        .set_len(3)
        .unwrap();
    let err = read.collect(7).await.unwrap_err();
    assert_eq!(err.kind(), BlobErrorKind::Io);
    assert_eq!(err.context().unwrap().digest.as_deref(), Some(digest.as_str()));
}

#[tokio::test]
async fn test_materialize_reports_a_lease_directory_error() {
    let dir = tempfile::tempdir().unwrap();
    let storage = BlobStorage::filesystem(dir.path());
    let digest = storage.put_bytes(b"package").await.unwrap();
    std::fs::write(dir.path().join(".leases"), b"not a directory").unwrap();
    assert_eq!(
        storage.materialize(&digest).await.unwrap_err().kind(),
        BlobErrorKind::Io
    );
}

#[cfg(unix)]
#[tokio::test]
async fn test_materialize_falls_back_when_a_hard_link_is_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let storage = BlobStorage::filesystem(dir.path());
    let digest = Digest::of(b"directory");
    let hex = digest.as_str();
    std::fs::create_dir_all(dir.path().join(format!("sha256/{}/{}/{}", &hex[..2], &hex[2..4], hex))).unwrap();
    assert_eq!(
        storage.materialize(&digest).await.unwrap_err().kind(),
        BlobErrorKind::Io
    );
}

#[tokio::test]
async fn test_stream_payload_collects_without_a_file_path() {
    let read = BlobRead::new(
        "stream",
        Digest::of(b"package"),
        peryx_storage::blob::BlobMetadata {
            bytes: 7,
            modified: None,
        },
        0..7,
        BlobReadBody::Stream(
            futures_util::stream::iter([
                Ok(bytes::Bytes::from_static(b"pack")),
                Ok(bytes::Bytes::from_static(b"age")),
            ])
            .boxed(),
        ),
    );
    assert_eq!(read.collect(7).await.unwrap(), b"package");
}

#[tokio::test]
async fn test_stream_payload_rejects_reversed_range() {
    let digest = Digest::of(b"package");
    let read = BlobRead::new(
        "stream",
        digest.clone(),
        peryx_storage::blob::BlobMetadata {
            bytes: 7,
            modified: None,
        },
        std::ops::Range { start: 5, end: 1 },
        BlobReadBody::Stream(futures_util::stream::empty().boxed()),
    );
    let err = read.collect(7).await.unwrap_err();
    assert_eq!(err.kind(), BlobErrorKind::InvalidRange);
    assert_eq!(err.invalid_range_values(), Some((5, 1, 7)));
    assert_eq!(err.context().unwrap().digest.as_deref(), Some(digest.as_str()));
}

#[tokio::test]
async fn test_stream_payload_rejects_short_body() {
    let digest = Digest::of(b"package");
    let read = BlobRead::new(
        "stream",
        digest.clone(),
        peryx_storage::blob::BlobMetadata {
            bytes: 7,
            modified: None,
        },
        0..7,
        BlobReadBody::Stream(futures_util::stream::once(async { Ok(bytes::Bytes::from_static(b"pack")) }).boxed()),
    );
    let err = read.collect(7).await.unwrap_err();
    assert_eq!(err.kind(), BlobErrorKind::Io);
    assert_eq!(err.context().unwrap().digest.as_deref(), Some(digest.as_str()));
    assert_eq!(
        std::error::Error::source(&err)
            .unwrap()
            .downcast_ref::<std::io::Error>()
            .unwrap()
            .kind(),
        std::io::ErrorKind::UnexpectedEof
    );
}

#[tokio::test]
async fn test_stream_payload_rejects_long_body() {
    let digest = Digest::of(b"package");
    let read = BlobRead::new(
        "stream",
        digest,
        peryx_storage::blob::BlobMetadata {
            bytes: 7,
            modified: None,
        },
        0..4,
        BlobReadBody::Stream(futures_util::stream::once(async { Ok(bytes::Bytes::from_static(b"package")) }).boxed()),
    );
    let err = read.collect(7).await.unwrap_err();
    assert_eq!(
        std::error::Error::source(&err)
            .unwrap()
            .downcast_ref::<std::io::Error>()
            .unwrap()
            .kind(),
        std::io::ErrorKind::InvalidData
    );
}

#[tokio::test]
async fn test_context_preserves_digest_mismatch_kind() {
    let dir = tempfile::tempdir().unwrap();
    let storage = BlobStorage::filesystem(dir.path());
    let expected = Digest::of(b"expected");
    let err = storage.put_bytes_as(b"other", &expected).await.unwrap_err();
    assert_eq!(err.kind(), BlobErrorKind::DigestMismatch);
    assert_eq!(
        err.mismatch().unwrap(),
        (expected.as_str(), Digest::of(b"other").as_str())
    );
    assert_eq!(err.context().unwrap().operation, BlobOperation::Commit);
}

#[tokio::test]
async fn test_context_preserves_range_and_not_found_kinds() {
    let dir = tempfile::tempdir().unwrap();
    let storage = BlobStorage::filesystem(dir.path());
    let digest = storage.put_bytes(b"package").await.unwrap();
    let range = storage.open(&digest, Some(3..9)).await.err().unwrap();
    assert_eq!(range.kind(), BlobErrorKind::InvalidRange);
    assert_eq!(range.invalid_range_values(), Some((3, 9, 7)));
    assert_eq!(range.context().unwrap().operation, BlobOperation::Open);
    let missing = Digest::of(b"missing");
    let err = storage.open(&missing, None).await.err().unwrap();
    assert_eq!(err.kind(), BlobErrorKind::NotFound);
    assert_eq!(err.context().unwrap().digest.as_deref(), Some(missing.as_str()));
}

#[test]
fn test_unsupported_error_is_typed_and_contextual() {
    let digest = Digest::of(b"blob");
    let err = BlobError::unsupported("delete").with_context("remote", BlobOperation::Delete, Some(&digest));
    assert_eq!(err.kind(), BlobErrorKind::Unsupported);
    assert_eq!(err.context().unwrap().backend, "remote");
    assert!(err.to_string().contains("delete is unsupported"));
}

#[tokio::test]
async fn test_display_does_not_expose_the_configured_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("secret-root");
    std::fs::write(&root, b"not a directory").unwrap();
    let err = BlobStorage::filesystem(&root).health().await.unwrap_err();
    assert_eq!(err.kind(), BlobErrorKind::Io);
    assert!(!err.to_string().contains(root.to_string_lossy().as_ref()));
}
