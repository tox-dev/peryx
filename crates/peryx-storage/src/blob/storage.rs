use std::collections::HashSet;
use std::ops::Range;

use super::{
    BlobBackend, BlobCapabilities, BlobEntry, BlobError, BlobLease, BlobMetadata, BlobOperation, BlobRead,
    BlobScanError, BlobStaged, BlobStore, BlobWrite, Digest,
};

/// The blob backend selected for this process.
#[derive(Debug, Clone)]
pub struct BlobStorage {
    backend: Backend,
}

#[derive(Debug, Clone)]
enum Backend {
    Filesystem(BlobStore),
}

fn filesystem_context<T>(
    result: Result<T, BlobError>,
    operation: BlobOperation,
    digest: Option<&Digest>,
) -> Result<T, BlobError> {
    match result {
        Ok(value) => Ok(value),
        Err(error) => Err(error.with_context("filesystem", operation, digest)),
    }
}

impl BlobStorage {
    /// Select the filesystem backend rooted at `root`.
    #[must_use]
    pub fn filesystem(root: impl Into<std::path::PathBuf>) -> Self {
        Self::from(BlobStore::new(root))
    }

    /// Stable backend name for status and error surfaces.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self.backend {
            Backend::Filesystem(_) => "filesystem",
        }
    }

    /// The effective configured backend contract.
    #[must_use]
    pub fn capabilities(&self) -> BlobCapabilities {
        match &self.backend {
            Backend::Filesystem(store) => store.capabilities(),
        }
    }

    /// Explicit blocking access for offline import and maintenance commands.
    #[must_use]
    pub const fn blocking(&self) -> BlobBlocking<'_> {
        BlobBlocking { backend: &self.backend }
    }

    /// Check that the configured backend is usable.
    ///
    /// # Errors
    /// Returns a contextual backend error when the check fails.
    pub async fn health(&self) -> Result<(), BlobError> {
        match &self.backend {
            Backend::Filesystem(store) => store.health().await,
        }
    }

    /// Open a whole blob or one end-exclusive byte range without collecting it.
    ///
    /// # Errors
    /// Returns a contextual not-found, range, or backend error.
    pub async fn open(&self, digest: &Digest, range: Option<Range<u64>>) -> Result<BlobRead, BlobError> {
        match &self.backend {
            Backend::Filesystem(store) => store.open(digest.clone(), range).await,
        }
    }

    /// Collect a blob only when its declared size fits `max_bytes`.
    ///
    /// # Errors
    /// Returns a contextual size, read, or backend error.
    pub async fn read_bytes(&self, digest: &Digest, max_bytes: u64) -> Result<Vec<u8>, BlobError> {
        self.open(digest, None)
            .await?
            .collect(max_bytes)
            .await
            .map_err(|error| error.with_context(self.name(), BlobOperation::Open, Some(digest)))
    }

    /// Read metadata without fetching blob contents.
    ///
    /// # Errors
    /// Returns a contextual backend error.
    pub async fn head(&self, digest: &Digest) -> Result<Option<BlobMetadata>, BlobError> {
        match &self.backend {
            Backend::Filesystem(store) => BlobBackend::head(store, digest.clone()).await,
        }
    }

    /// Check many content addresses in one backend task.
    ///
    /// # Errors
    /// Returns a contextual metadata error.
    ///
    /// # Panics
    /// Panics if the internal blocking task panics.
    pub async fn present(&self, digests: Vec<Digest>) -> Result<HashSet<Digest>, BlobError> {
        match &self.backend {
            Backend::Filesystem(store) => {
                let store = store.clone();
                tokio::task::spawn_blocking(move || {
                    let mut present = HashSet::with_capacity(digests.len());
                    for digest in digests {
                        if filesystem_context(store.head(&digest), BlobOperation::Head, Some(&digest))?.is_some() {
                            present.insert(digest);
                        }
                    }
                    Ok::<_, BlobError>(present)
                })
                .await
                .expect("blob presence task never panics")
            }
        }
    }

    /// Begin a streamed write.
    ///
    /// # Errors
    /// Returns a contextual backend error when staging cannot start.
    pub async fn begin(&self) -> Result<BlobWrite, BlobError> {
        match &self.backend {
            Backend::Filesystem(store) => BlobBackend::begin(store).await,
        }
    }

    /// Stage bytes already held in memory.
    ///
    /// # Errors
    /// Returns a contextual write error.
    pub async fn stage_bytes(&self, bytes: &[u8]) -> Result<BlobStaged, BlobError> {
        let mut write = self.begin().await?;
        write.write_chunk(bytes::Bytes::copy_from_slice(bytes)).await?;
        write.finish().await
    }

    /// Persist bytes already held in memory and return their digest.
    ///
    /// # Errors
    /// Returns a contextual write or commit error.
    pub async fn put_bytes(&self, bytes: &[u8]) -> Result<Digest, BlobError> {
        let staged = self.stage_bytes(bytes).await?;
        let digest = staged.digest().clone();
        staged.commit().await?;
        Ok(digest)
    }

    /// Persist in-memory bytes only at the expected content address.
    ///
    /// # Errors
    /// Returns a contextual digest mismatch, write, or commit error.
    pub async fn put_bytes_as(&self, bytes: &[u8], expected: &Digest) -> Result<(), BlobError> {
        self.stage_bytes(bytes).await?.commit_as(expected).await
    }

    /// Verify stored bytes against their address.
    ///
    /// # Errors
    /// Returns a contextual not-found or backend error.
    pub async fn verify(&self, digest: &Digest) -> Result<bool, BlobError> {
        match &self.backend {
            Backend::Filesystem(store) => BlobBackend::verify(store, digest.clone()).await,
        }
    }

    /// Delete a blob, reporting whether it existed.
    ///
    /// # Errors
    /// Returns a contextual backend error.
    pub async fn delete(&self, digest: &Digest) -> Result<bool, BlobError> {
        match &self.backend {
            Backend::Filesystem(store) => store.delete(digest.clone()).await,
        }
    }

    /// Hold a seekable local representation for archive or backup work.
    ///
    /// # Errors
    /// Returns a contextual not-found or materialization error.
    pub async fn materialize(&self, digest: &Digest) -> Result<BlobLease, BlobError> {
        match &self.backend {
            Backend::Filesystem(store) => store.materialize(digest.clone()).await,
        }
    }
}

/// Blocking blob operations kept out of protocol request paths.
pub struct BlobBlocking<'storage> {
    backend: &'storage Backend,
}

impl BlobBlocking<'_> {
    /// Stage bytes from a blocking reader.
    ///
    /// # Errors
    /// Returns a contextual read or staging error.
    pub fn stage_reader(&self, reader: &mut dyn std::io::Read) -> Result<BlobStaged, BlobError> {
        match self.backend {
            Backend::Filesystem(store) => {
                let mut pending = filesystem_context(store.begin(), BlobOperation::Write, None)?;
                let mut buffer = vec![0; 1024 * 1024];
                loop {
                    let read = reader.read(&mut buffer).map_err(BlobError::from)?;
                    if read == 0 {
                        break;
                    }
                    filesystem_context(pending.write(&buffer[..read]), BlobOperation::Write, None)?;
                }
                let staged = filesystem_context(pending.finish(), BlobOperation::Write, None)?;
                Ok(BlobStaged::filesystem(store.clone(), staged))
            }
        }
    }

    /// Stage bytes already held in memory.
    ///
    /// # Errors
    /// Returns a contextual staging error.
    pub fn stage_bytes(&self, bytes: &[u8]) -> Result<BlobStaged, BlobError> {
        self.stage_reader(&mut std::io::Cursor::new(bytes))
    }

    /// Publish a blocking stage.
    ///
    /// # Errors
    /// Returns a contextual commit error.
    pub fn commit(&self, staged: BlobStaged) -> Result<(), BlobError> {
        staged.commit_blocking()
    }

    /// Publish a blocking stage only at the expected digest.
    ///
    /// # Errors
    /// Returns a contextual mismatch or commit error.
    pub fn commit_as(&self, staged: BlobStaged, expected: &Digest) -> Result<(), BlobError> {
        staged.commit_as_blocking(expected)
    }

    /// Read metadata without fetching bytes.
    ///
    /// # Errors
    /// Returns a contextual backend error.
    pub fn head(&self, digest: &Digest) -> Result<Option<BlobMetadata>, BlobError> {
        match self.backend {
            Backend::Filesystem(store) => filesystem_context(store.head(digest), BlobOperation::Head, Some(digest)),
        }
    }

    /// Collect an already bounded blob.
    ///
    /// # Errors
    /// Returns a contextual size or read error.
    pub fn read_bytes(&self, digest: &Digest, max_bytes: u64) -> Result<Vec<u8>, BlobError> {
        match self.backend {
            Backend::Filesystem(store) => {
                let mut file = std::fs::File::open(store.path_for(digest)).map_err(|error| {
                    let error = if error.kind() == std::io::ErrorKind::NotFound {
                        BlobError::not_found(digest)
                    } else {
                        error.into()
                    };
                    error.with_context("filesystem", BlobOperation::Open, Some(digest))
                })?;
                let metadata = file.metadata().map_err(BlobError::from);
                let bytes = filesystem_context(metadata, BlobOperation::Open, Some(digest))?.len();
                if bytes > max_bytes {
                    return Err(BlobError::limit_exceeded(max_bytes, bytes).with_context(
                        "filesystem",
                        BlobOperation::Open,
                        Some(digest),
                    ));
                }
                #[cfg(target_pointer_width = "64")]
                let length = usize::try_from(bytes).unwrap_or(usize::MAX);
                #[cfg(not(target_pointer_width = "64"))]
                let length = bytes.try_into().map_err(|_| {
                    BlobError::limit_exceeded(usize::MAX as u64, bytes).with_context(
                        "filesystem",
                        BlobOperation::Open,
                        Some(digest),
                    )
                })?;
                let mut result = vec![0; length];
                let read = std::io::Read::read_exact(&mut file, &mut result).map_err(BlobError::from);
                filesystem_context(read, BlobOperation::Open, Some(digest))?;
                Ok(result)
            }
        }
    }

    /// Persist bytes already held in memory.
    ///
    /// # Errors
    /// Returns a contextual write or commit error.
    pub fn put_bytes(&self, bytes: &[u8]) -> Result<Digest, BlobError> {
        let staged = self.stage_bytes(bytes)?;
        let digest = staged.digest().clone();
        self.commit(staged)?;
        Ok(digest)
    }

    /// Persist in-memory bytes only at the expected content address.
    ///
    /// # Errors
    /// Returns a contextual digest mismatch, write, or commit error.
    pub fn put_bytes_as(&self, bytes: &[u8], expected: &Digest) -> Result<(), BlobError> {
        self.commit_as(self.stage_bytes(bytes)?, expected)
    }

    /// Hold a seekable local representation.
    ///
    /// # Errors
    /// Returns a contextual materialization error.
    pub fn materialize(&self, digest: &Digest) -> Result<BlobLease, BlobError> {
        match self.backend {
            Backend::Filesystem(store) => {
                let path = store.path_for(digest);
                BlobLease::pinned(&path, &store.lease_dir()).map_err(|error| {
                    let error = if error.kind() == std::io::ErrorKind::NotFound {
                        BlobError::not_found(digest)
                    } else {
                        error.into()
                    };
                    error.with_context("filesystem", BlobOperation::Materialize, Some(digest))
                })
            }
        }
    }

    /// Verify a stored blob.
    ///
    /// # Errors
    /// Returns a contextual read error.
    pub fn verify(&self, digest: &Digest) -> Result<bool, BlobError> {
        match self.backend {
            Backend::Filesystem(store) => store
                .verify(digest)
                .map_err(|error| error.with_context("filesystem", BlobOperation::Verify, Some(digest))),
        }
    }

    /// Delete a stored blob.
    ///
    /// # Errors
    /// Returns a contextual delete error.
    pub fn delete(&self, digest: &Digest) -> Result<bool, BlobError> {
        match self.backend {
            Backend::Filesystem(store) => filesystem_context(store.remove(digest), BlobOperation::Delete, Some(digest)),
        }
    }

    /// Visit backend entries without collecting them.
    ///
    /// # Errors
    /// Returns a contextual listing or visitor error.
    pub fn visit<E>(&self, visit: impl FnMut(BlobEntry) -> Result<(), E>) -> Result<(), BlobScanError<E>> {
        match self.backend {
            Backend::Filesystem(store) => store.scan(visit).map_err(|error| match error {
                BlobScanError::Store(error) => {
                    BlobScanError::Store(error.with_context("filesystem", BlobOperation::List, None))
                }
                BlobScanError::Visit(error) => BlobScanError::Visit(error),
            }),
        }
    }
}

impl From<BlobStore> for BlobStorage {
    fn from(store: BlobStore) -> Self {
        Self {
            backend: Backend::Filesystem(store),
        }
    }
}

impl BlobBackend for BlobStorage {
    fn capabilities(&self) -> BlobCapabilities {
        Self::capabilities(self)
    }

    async fn health(&self) -> Result<(), BlobError> {
        Self::health(self).await
    }

    async fn open(&self, digest: Digest, range: Option<Range<u64>>) -> Result<BlobRead, BlobError> {
        Self::open(self, &digest, range).await
    }

    async fn head(&self, digest: Digest) -> Result<Option<BlobMetadata>, BlobError> {
        Self::head(self, &digest).await
    }

    async fn begin(&self) -> Result<BlobWrite, BlobError> {
        Self::begin(self).await
    }

    async fn verify(&self, digest: Digest) -> Result<bool, BlobError> {
        Self::verify(self, &digest).await
    }

    async fn delete(&self, digest: Digest) -> Result<bool, BlobError> {
        Self::delete(self, &digest).await
    }

    async fn materialize(&self, digest: Digest) -> Result<BlobLease, BlobError> {
        Self::materialize(self, &digest).await
    }
}
