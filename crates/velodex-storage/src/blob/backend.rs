use super::Digest;
use super::error::BlobError;
use super::store::BlobStore;

/// A content-addressed blob storage backend.
///
/// Package files are stored by their sha256 [`Digest`], so writes are immutable and the digest is the
/// only key. This trait is the seam a storage backend plugs into. The filesystem backend
/// ([`BlobStore`]) covers local disk and any mounted filesystem (NFS needs no separate code, it is
/// just a different mount), and an S3-compatible object-store backend is a future implementation.
///
/// Backends are dispatched **statically**: the running store is one concrete type today, and becomes a
/// small enum matched per call once a second backend exists, never a boxed trait object on the
/// request path. So routing blob reads and writes through this trait costs nothing over calling the
/// filesystem store directly.
///
/// Streaming staged writes and on-disk range serving remain [`BlobStore`]-specific for now; they
/// generalize alongside the object-store backend, whose multipart upload and range-get shape the
/// backend-agnostic contract.
pub trait BlobBackend {
    /// Whether a blob with this digest is stored.
    fn exists(&self, digest: &Digest) -> bool;

    /// Read a whole blob by digest.
    ///
    /// # Errors
    /// Returns [`BlobError`] if the blob is missing or cannot be read.
    fn read(&self, digest: &Digest) -> Result<Vec<u8>, BlobError>;

    /// Store bytes and return their digest. Immutable: writing an already-stored digest is a no-op.
    ///
    /// # Errors
    /// Returns [`BlobError`] if the bytes cannot be written.
    fn write(&self, bytes: &[u8]) -> Result<Digest, BlobError>;

    /// Store bytes whose digest must equal `expected`.
    ///
    /// # Errors
    /// Returns [`BlobError`] on a digest mismatch or a write failure.
    fn write_verified(&self, bytes: &[u8], expected: &Digest) -> Result<(), BlobError>;

    /// Re-hash a stored blob and report whether it still matches its digest.
    ///
    /// # Errors
    /// Returns [`BlobError`] if the blob is missing or cannot be read.
    fn verify(&self, digest: &Digest) -> Result<bool, BlobError>;

    /// Delete a blob, returning whether it existed.
    ///
    /// # Errors
    /// Returns [`BlobError`] if the blob exists but cannot be removed.
    fn remove(&self, digest: &Digest) -> Result<bool, BlobError>;
}

impl BlobBackend for BlobStore {
    fn exists(&self, digest: &Digest) -> bool {
        Self::exists(self, digest)
    }

    fn read(&self, digest: &Digest) -> Result<Vec<u8>, BlobError> {
        Self::read(self, digest)
    }

    fn write(&self, bytes: &[u8]) -> Result<Digest, BlobError> {
        Self::write(self, bytes)
    }

    fn write_verified(&self, bytes: &[u8], expected: &Digest) -> Result<(), BlobError> {
        Self::write_verified(self, bytes, expected)
    }

    fn verify(&self, digest: &Digest) -> Result<bool, BlobError> {
        Self::verify(self, digest)
    }

    fn remove(&self, digest: &Digest) -> Result<bool, BlobError> {
        Self::remove(self, digest)
    }
}
