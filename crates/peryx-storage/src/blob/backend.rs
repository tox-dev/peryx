use std::future::Future;
use std::ops::Range;

use super::BlobMetadata;
use super::Digest;
use super::error::{BlobError, BlobOperation};
use super::store::BlobStore;

/// A content-addressed blob storage backend.
///
/// Package files are stored by their sha256 [`Digest`], so writes are immutable and the digest is the
/// only key. This trait is the seam a storage backend plugs into. The filesystem backend
/// ([`BlobStore`]) covers local disk and any mounted filesystem (NFS needs no separate code, it is
/// just a different mount), and an S3-compatible object-store backend is a future implementation.
///
/// The contract owns request inputs and returns `Send` futures so network backends can perform I/O
/// without blocking an executor thread. Callers can use static dispatch or wrap a fixed set of
/// backends in an enum without allocating a boxed trait object per request.
pub trait BlobBackend: Send + Sync {
    /// Store bytes at their immutable digest key.
    ///
    /// # Errors
    /// Returns [`BlobError`] if the bytes do not match `digest` or the backend cannot store them.
    fn put(&self, digest: Digest, bytes: Vec<u8>) -> impl Future<Output = Result<(), BlobError>> + Send;

    /// Read a whole blob.
    ///
    /// # Errors
    /// Returns [`BlobError`] if the blob is missing or cannot be read.
    fn get(&self, digest: Digest) -> impl Future<Output = Result<Vec<u8>, BlobError>> + Send;

    /// Return a blob's metadata without reading its contents.
    ///
    /// # Errors
    /// Returns [`BlobError`] if the backend cannot determine whether the blob exists.
    fn head(&self, digest: Digest) -> impl Future<Output = Result<Option<BlobMetadata>, BlobError>> + Send;

    /// Read an end-exclusive byte range.
    ///
    /// # Errors
    /// Returns [`BlobError`] if the blob is missing, the range is invalid, or the read fails.
    fn range(&self, digest: Digest, range: Range<u64>) -> impl Future<Output = Result<Vec<u8>, BlobError>> + Send;

    /// Re-hash a stored blob and report whether it still matches its digest.
    ///
    /// # Errors
    /// Returns [`BlobError`] if the blob is missing or cannot be read.
    fn verify(&self, digest: Digest) -> impl Future<Output = Result<bool, BlobError>> + Send;

    /// Delete a blob, returning whether it existed.
    ///
    /// # Errors
    /// Returns [`BlobError`] if the blob exists but cannot be removed.
    fn delete(&self, digest: Digest) -> impl Future<Output = Result<bool, BlobError>> + Send;
}

impl BlobBackend for BlobStore {
    async fn put(&self, digest: Digest, bytes: Vec<u8>) -> Result<(), BlobError> {
        run(self.clone(), digest, BlobOperation::Put, move |store, digest| {
            store.write_verified(&bytes, &digest)
        })
        .await
    }

    async fn get(&self, digest: Digest) -> Result<Vec<u8>, BlobError> {
        run(self.clone(), digest, BlobOperation::Get, |store, digest| {
            store.read(&digest)
        })
        .await
    }

    async fn head(&self, digest: Digest) -> Result<Option<BlobMetadata>, BlobError> {
        run(self.clone(), digest, BlobOperation::Head, |store, digest| {
            store.head(&digest)
        })
        .await
    }

    async fn range(&self, digest: Digest, range: Range<u64>) -> Result<Vec<u8>, BlobError> {
        run(self.clone(), digest, BlobOperation::Range, move |store, digest| {
            store.read_range(&digest, range)
        })
        .await
    }

    async fn verify(&self, digest: Digest) -> Result<bool, BlobError> {
        run(self.clone(), digest, BlobOperation::Verify, |store, digest| {
            store.verify(&digest)
        })
        .await
    }

    async fn delete(&self, digest: Digest) -> Result<bool, BlobError> {
        run(self.clone(), digest, BlobOperation::Delete, |store, digest| {
            store.remove(&digest)
        })
        .await
    }
}

async fn run<T, E>(
    store: BlobStore,
    digest: Digest,
    operation: BlobOperation,
    action: impl FnOnce(BlobStore, Digest) -> Result<T, E> + Send + 'static,
) -> Result<T, BlobError>
where
    T: Send + 'static,
    E: std::error::Error + Send + Sync + 'static,
{
    let error_digest = digest.clone();
    tokio::task::spawn_blocking(move || action(store, digest))
        .await
        .expect("filesystem blob task must not panic")
        .map_err(|source| BlobError::backend("filesystem", operation, &error_digest, source))
}
