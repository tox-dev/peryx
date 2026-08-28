use std::future::Future;
use std::io::{Read as _, Seek as _};
use std::ops::Range;
use std::path::{Path, PathBuf};

use bytes::Bytes;
use futures_util::stream::BoxStream;
use futures_util::{StreamExt as _, TryStreamExt as _};
use peryx_core::BlobDurability;

use super::error::{BlobError, BlobOperation};
use super::store::{BlobStore, PendingBlob, StagedBlob};
use super::{BlobMetadata, Digest, PlacementReceipt};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobSupport {
    Native,
    Emulated,
    Unsupported,
}

impl BlobSupport {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Emulated => "emulated",
            Self::Unsupported => "unsupported",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobCapabilities {
    pub durability: BlobDurability,
    pub create_if_absent: BlobSupport,
    pub range: BlobSupport,
    pub checksum: BlobSupport,
    pub delete: BlobSupport,
    pub list: BlobSupport,
    pub local_tail: BlobSupport,
}

pub struct BlobRead {
    pub metadata: BlobMetadata,
    pub range: Range<u64>,
    pub body: BlobReadBody,
    backend: &'static str,
    digest: Digest,
}

impl BlobRead {
    /// Retains backend context for errors raised while consuming the stream.
    #[must_use]
    pub fn new(
        backend: &'static str,
        digest: Digest,
        metadata: BlobMetadata,
        range: Range<u64>,
        body: BlobReadBody,
    ) -> Self {
        let body = match body {
            BlobReadBody::File(file) => BlobReadBody::File(file),
            BlobReadBody::Stream(stream) => {
                let error = BlobError::invalid_range(range.start, range.end, metadata.bytes).with_context(
                    backend,
                    BlobOperation::Open,
                    Some(&digest),
                );
                range.end.checked_sub(range.start).map_or_else(
                    || BlobReadBody::Stream(futures_util::stream::iter([Err(error)]).boxed()),
                    |expected| BlobReadBody::Stream(checked_stream(stream, expected, backend, digest.clone())),
                )
            }
        };
        Self {
            metadata,
            range,
            body,
            backend,
            digest,
        }
    }

    /// Rejects a declared size above `max_bytes` before reading the payload.
    ///
    /// # Errors
    /// Returns a limit, range, or payload-read error.
    ///
    pub async fn collect(self, max_bytes: u64) -> Result<Vec<u8>, BlobError> {
        let Self {
            metadata,
            range,
            body,
            backend,
            digest,
        } = self;
        let Some(expected) = range.end.checked_sub(range.start) else {
            return Err(
                BlobError::invalid_range(range.start, range.end, metadata.bytes).with_context(
                    backend,
                    BlobOperation::Open,
                    Some(&digest),
                ),
            );
        };
        if expected > max_bytes {
            return Err(BlobError::limit_exceeded(max_bytes, expected).with_context(
                backend,
                BlobOperation::Open,
                Some(&digest),
            ));
        }
        let result = match body {
            BlobReadBody::File(mut file) => tokio::task::spawn_blocking(move || {
                file.seek(std::io::SeekFrom::Start(range.start))?;
                let mut bytes = Vec::new();
                file.take(expected).read_to_end(&mut bytes)?;
                if bytes.len() as u64 != expected {
                    return Err(BlobError::io(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        format!("blob file declared {expected} bytes but yielded {}", bytes.len()),
                    )));
                }
                Ok::<_, BlobError>(bytes)
            })
            .await
            .map_err(|error| BlobError::from(error).with_context(backend, BlobOperation::Open, Some(&digest)))?,
            BlobReadBody::Stream(stream) => {
                stream
                    .try_fold(Vec::new(), |mut bytes, chunk| async move {
                        bytes.extend_from_slice(&chunk);
                        Ok(bytes)
                    })
                    .await
            }
        };
        result.map_err(|error| error.with_context(backend, BlobOperation::Open, Some(&digest)))
    }
}

fn checked_stream(
    stream: BoxStream<'static, Result<Bytes, BlobError>>,
    expected: u64,
    backend: &'static str,
    digest: Digest,
) -> BoxStream<'static, Result<Bytes, BlobError>> {
    futures_util::stream::try_unfold((stream, 0u64), move |(mut stream, received)| {
        let digest = digest.clone();
        async move {
            let Some(chunk) = stream.try_next().await? else {
                if received == expected {
                    return Ok(None);
                }
                return Err(BlobError::io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    format!("blob stream declared {expected} bytes but yielded {received}"),
                ))
                .with_context(backend, BlobOperation::Open, Some(&digest)));
            };
            let actual = received.saturating_add(chunk.len() as u64);
            if actual > expected {
                return Err(BlobError::io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("blob stream declared {expected} bytes but yielded at least {actual}"),
                ))
                .with_context(backend, BlobOperation::Open, Some(&digest)));
            }
            Ok(Some((chunk, (stream, actual))))
        }
    })
    .boxed()
}

pub enum BlobReadBody {
    File(std::fs::File),
    Stream(BoxStream<'static, Result<Bytes, BlobError>>),
}

pub struct BlobWrite {
    backend: BlobWriteBackend,
}

enum BlobWriteBackend {
    Filesystem(FilesystemWrite),
    S3(super::s3::S3Write),
}

struct FilesystemWrite {
    store: BlobStore,
    pending: Option<PendingBlob>,
    task: Option<tokio::task::JoinHandle<Result<PendingBlob, BlobError>>>,
    queued: Vec<Bytes>,
    queued_bytes: usize,
    tail: BlobTail,
}

const WRITE_BATCH_BYTES: usize = 1_048_576;

#[derive(Debug)]
pub struct BlobStaged {
    backend: Option<BlobStagedBackend>,
}

#[derive(Debug)]
enum BlobStagedBackend {
    Filesystem { store: BlobStore, staged: StagedBlob },
    S3(Box<super::s3::S3Staged>),
}

impl BlobWrite {
    pub(crate) fn filesystem(store: BlobStore) -> Result<Self, BlobError> {
        let pending = store
            .begin()
            .map_err(|error| error.with_context("filesystem", BlobOperation::Write, None))?;
        let tail = BlobTail {
            path: pending.path().to_owned(),
        };
        Ok(Self {
            backend: BlobWriteBackend::Filesystem(FilesystemWrite {
                store,
                pending: Some(pending),
                task: None,
                queued: Vec::new(),
                queued_bytes: 0,
                tail,
            }),
        })
    }

    pub(crate) const fn s3(write: super::s3::S3Write) -> Self {
        Self {
            backend: BlobWriteBackend::S3(write),
        }
    }

    /// # Errors
    /// Returns a contextual write error when the backend rejects the chunk.
    pub async fn write_chunk(&mut self, chunk: Bytes) -> Result<(), BlobError> {
        match &mut self.backend {
            BlobWriteBackend::Filesystem(write) => write.write_chunk(chunk).await,
            BlobWriteBackend::S3(write) => write.write_chunk(chunk).await,
        }
    }

    /// Makes accepted filesystem bytes visible to tail readers.
    ///
    /// # Errors
    /// Returns a contextual write error when the backend cannot flush the stage.
    pub async fn flush(&mut self) -> Result<u64, BlobError> {
        match &mut self.backend {
            BlobWriteBackend::Filesystem(write) => write.flush().await,
            BlobWriteBackend::S3(write) => write.flush().await,
        }
    }

    #[must_use]
    pub fn tail(&self) -> Option<BlobTail> {
        match &self.backend {
            BlobWriteBackend::Filesystem(write) => Some(write.tail.clone()),
            BlobWriteBackend::S3(write) => write.tail(),
        }
    }

    /// Publishes only when the stream hashes to `expected` and returns its durability evidence.
    ///
    /// # Errors
    /// Returns a digest mismatch or storage error without a receipt.
    pub async fn commit(self, expected: &Digest) -> Result<PlacementReceipt, BlobError> {
        match self.backend {
            BlobWriteBackend::Filesystem(_) => self.finish().await?.commit_as(expected).await,
            BlobWriteBackend::S3(write) => write.commit(expected).await,
        }
    }

    /// Syncs and hashes the stage without publishing it.
    ///
    /// # Errors
    /// Returns a contextual write error when the stage cannot be flushed or synced.
    pub async fn finish(self) -> Result<BlobStaged, BlobError> {
        match self.backend {
            BlobWriteBackend::Filesystem(write) => write.finish().await,
            BlobWriteBackend::S3(write) => write.finish().await,
        }
    }

    /// Waits for accepted writes before removing the unpublished stage.
    ///
    /// # Errors
    /// Returns a contextual write error when an accepted batch failed.
    pub async fn abort(self) -> Result<(), BlobError> {
        match self.backend {
            BlobWriteBackend::Filesystem(write) => write.abort().await,
            BlobWriteBackend::S3(write) => write.abort().await,
        }
    }
}

impl FilesystemWrite {
    async fn write_chunk(&mut self, chunk: Bytes) -> Result<(), BlobError> {
        self.settle().await?;
        if chunk.is_empty() {
            return Ok(());
        }
        let start = self.queued_bytes.saturating_add(chunk.len()) >= WRITE_BATCH_BYTES;
        if start {
            let permit = self.store.worker_permit().await;
            self.queue(chunk);
            self.start_batch(false, permit);
        } else {
            self.queue(chunk);
        }
        Ok(())
    }

    fn queue(&mut self, chunk: Bytes) {
        self.queued_bytes = self.queued_bytes.saturating_add(chunk.len());
        self.queued.push(chunk);
    }

    async fn flush(&mut self) -> Result<u64, BlobError> {
        self.settle().await?;
        let store = self.store.clone();
        self.start_batch(true, store.worker_permit().await);
        self.settle().await?;
        Ok(self.pending.as_ref().expect("settled writer retains its stage").len())
    }

    async fn finish(mut self) -> Result<BlobStaged, BlobError> {
        self.settle().await?;
        let permit = self.store.worker_permit().await;
        let pending = self.pending.take().expect("settled writer retains its stage");
        let queued = std::mem::take(&mut self.queued);
        let store = self.store.clone();
        let staged = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let mut pending = pending;
            queued.into_iter().try_for_each(|chunk| pending.write(&chunk))?;
            pending.finish()
        })
        .await
        .map_err(|error| BlobError::from(error).with_context("filesystem", BlobOperation::Write, None))?;
        let staged = filesystem_context(staged, BlobOperation::Write, None)?;
        Ok(BlobStaged::filesystem(store, staged))
    }

    async fn abort(mut self) -> Result<(), BlobError> {
        self.settle().await?;
        let permit = self.store.worker_permit().await;
        let pending = self.pending.take().expect("settled writer retains its stage");
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            pending.abort()
        })
        .await
        .map_err(|error| BlobError::from(error).with_context("filesystem", BlobOperation::Write, None))?
        .map_err(|error| error.with_context("filesystem", BlobOperation::Write, None))
    }

    fn start_batch(&mut self, flush: bool, permit: tokio::sync::OwnedSemaphorePermit) {
        let pending = self.pending.take().expect("settled writer retains its stage");
        let queued = std::mem::take(&mut self.queued);
        self.queued_bytes = 0;
        self.task = Some(tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let mut pending = pending;
            queued
                .into_iter()
                .try_for_each(|chunk| pending.write(&chunk))
                .and_then(|()| if flush { pending.flush() } else { Ok(()) })?;
            Ok(pending)
        }));
    }

    async fn settle(&mut self) -> Result<(), BlobError> {
        let Some(task) = self.task.take() else {
            return Ok(());
        };
        let pending = task
            .await
            .map_err(|error| BlobError::from(error).with_context("filesystem", BlobOperation::Write, None))?;
        self.pending = Some(filesystem_context(pending, BlobOperation::Write, None)?);
        Ok(())
    }
}

impl BlobStaged {
    pub(crate) const fn filesystem(store: BlobStore, staged: StagedBlob) -> Self {
        Self {
            backend: Some(BlobStagedBackend::Filesystem { store, staged }),
        }
    }

    pub(crate) fn s3(staged: super::s3::S3Staged) -> Self {
        Self {
            backend: Some(BlobStagedBackend::S3(Box::new(staged))),
        }
    }

    #[must_use]
    pub const fn digest(&self) -> &Digest {
        match self.backend() {
            BlobStagedBackend::Filesystem { staged, .. } => staged.digest(),
            BlobStagedBackend::S3(staged) => staged.digest(),
        }
    }

    #[must_use]
    pub const fn len(&self) -> u64 {
        match self.backend() {
            BlobStagedBackend::Filesystem { staged, .. } => staged.len(),
            BlobStagedBackend::S3(staged) => staged.len(),
        }
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        match self.backend() {
            BlobStagedBackend::Filesystem { staged, .. } => staged.is_empty(),
            BlobStagedBackend::S3(staged) => staged.is_empty(),
        }
    }

    pub fn with_materialized<T>(&self, inspect: impl FnOnce(&Path) -> T) -> T {
        match self.backend() {
            BlobStagedBackend::Filesystem { staged, .. } => inspect(staged.path()),
            BlobStagedBackend::S3(staged) => staged.inner().with_materialized(inspect),
        }
    }

    /// Publishes the verified stage and returns its durability evidence.
    ///
    /// # Errors
    /// Returns a storage error without a receipt.
    ///
    pub async fn commit(mut self) -> Result<PlacementReceipt, BlobError> {
        match self.take_backend() {
            BlobStagedBackend::Filesystem { store, staged } => {
                let permit = store.worker_permit().await;
                tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    Self::commit_backend(BlobStagedBackend::Filesystem { store, staged })
                })
                .await
                .map_err(|error| BlobError::from(error).with_context("filesystem", BlobOperation::Commit, None))?
            }
            BlobStagedBackend::S3(staged) => Box::pin(staged.commit()).await,
        }
    }

    pub(crate) fn commit_blocking(mut self) -> Result<PlacementReceipt, BlobError> {
        let backend = self.take_backend();
        Self::commit_backend(backend)
    }

    /// Publishes only when the computed address matches `expected`.
    ///
    /// # Errors
    /// Returns a digest mismatch or commit error without a receipt.
    pub async fn commit_as(self, expected: &Digest) -> Result<PlacementReceipt, BlobError> {
        if self.digest() != expected {
            let error = BlobError::digest_mismatch(expected, self.digest()).with_context(
                self.backend_name(),
                BlobOperation::Commit,
                Some(expected),
            );
            self.abort().await?;
            return Err(error);
        }
        self.commit().await
    }

    pub(crate) fn commit_as_blocking(self, expected: &Digest) -> Result<PlacementReceipt, BlobError> {
        if self.digest() != expected {
            let error = BlobError::digest_mismatch(expected, self.digest()).with_context(
                self.backend_name(),
                BlobOperation::Commit,
                Some(expected),
            );
            self.abort_blocking()?;
            return Err(error);
        }
        self.commit_blocking()
    }

    /// # Errors
    /// Returns a contextual cleanup error.
    ///
    pub async fn abort(mut self) -> Result<(), BlobError> {
        match self.take_backend() {
            BlobStagedBackend::Filesystem { store, staged } => {
                let permit = store.worker_permit().await;
                tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    Self::abort_backend(BlobStagedBackend::Filesystem { store, staged })
                })
                .await
                .map_err(|error| BlobError::from(error).with_context("filesystem", BlobOperation::Write, None))?
            }
            BlobStagedBackend::S3(staged) => staged.abort().await,
        }
    }

    fn abort_blocking(mut self) -> Result<(), BlobError> {
        let backend = self.take_backend();
        Self::abort_backend(backend)
    }

    const fn backend(&self) -> &BlobStagedBackend {
        self.backend.as_ref().expect("staged blob retains its backend")
    }

    const fn backend_name(&self) -> &'static str {
        match self.backend() {
            BlobStagedBackend::Filesystem { .. } => "filesystem",
            BlobStagedBackend::S3(_) => "s3",
        }
    }

    const fn take_backend(&mut self) -> BlobStagedBackend {
        self.backend.take().expect("staged blob retains its backend")
    }

    fn commit_backend(backend: BlobStagedBackend) -> Result<PlacementReceipt, BlobError> {
        match backend {
            BlobStagedBackend::Filesystem { store, staged } => {
                let digest = staged.digest().clone();
                filesystem_context(store.commit_staged(staged), BlobOperation::Commit, Some(&digest))
            }
            BlobStagedBackend::S3(_) => Err(
                BlobError::unsupported("blocking commit on the s3 backend").with_context(
                    "s3",
                    BlobOperation::Commit,
                    None,
                ),
            ),
        }
    }

    fn abort_backend(backend: BlobStagedBackend) -> Result<(), BlobError> {
        match backend {
            BlobStagedBackend::Filesystem { staged, .. } => staged
                .abort()
                .map_err(|error| error.with_context("filesystem", BlobOperation::Write, None)),
            // Dropping the local stage removes it; an uncommitted write created no remote object.
            BlobStagedBackend::S3(_) => Ok(()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct BlobTail {
    path: PathBuf,
}

impl BlobTail {
    /// # Errors
    /// Returns an I/O error if the stage has already moved or cannot be read.
    pub fn open(&self) -> std::io::Result<std::fs::File> {
        std::fs::File::open(&self.path)
    }
}

#[derive(Debug)]
pub struct BlobLease {
    path: PathBuf,
    guard: LeaseGuard,
}

#[derive(Debug)]
enum LeaseGuard {
    Filesystem {
        lock: std::fs::File,
        coordination: std::fs::File,
        _temporary: tempfile::TempPath,
    },
    Downloaded {
        _temporary: tempfile::TempPath,
    },
}

impl BlobLease {
    /// Keeps the downloaded file until the lease drops.
    pub(crate) fn downloaded(temporary: tempfile::TempPath) -> Self {
        Self {
            path: temporary.to_path_buf(),
            guard: LeaseGuard::Downloaded { _temporary: temporary },
        }
    }

    pub(crate) fn pinned(path: &Path, lease_dir: &Path) -> Result<Self, std::io::Error> {
        Self::pinned_with(path, lease_dir, &|source, destination| {
            std::fs::hard_link(source, destination)
        })
    }

    fn pinned_with(
        path: &Path,
        lease_dir: &Path,
        hard_link: &dyn Fn(&Path, &Path) -> Result<(), std::io::Error>,
    ) -> Result<Self, std::io::Error> {
        std::fs::create_dir_all(lease_dir)?;
        let coordination = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(lease_dir.join(".cleanup.lock"))?;
        fs4::FileExt::lock_shared(&coordination)?;
        let mut source = std::fs::File::open(path)?;
        let temporary = tempfile::Builder::new()
            .prefix(".peryx-lease-")
            .tempfile_in(lease_dir)?
            .into_temp_path();
        std::fs::remove_file(&temporary)?;
        let lock = if hard_link(path, &temporary).is_ok() {
            source
        } else {
            let mut copy = std::fs::OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .open(&temporary)?;
            std::io::copy(&mut source, &mut copy)?;
            copy
        };
        fs4::FileExt::lock_shared(&lock)?;
        fs4::FileExt::unlock(&coordination)?;
        Ok(Self {
            path: temporary.to_path_buf(),
            guard: LeaseGuard::Filesystem {
                lock,
                coordination,
                _temporary: temporary,
            },
        })
    }

    /// The returned path remains valid only for the lease's lifetime.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for BlobLease {
    fn drop(&mut self) {
        if let LeaseGuard::Filesystem { lock, coordination, .. } = &self.guard {
            let _ = fs4::FileExt::lock_shared(coordination);
            let _ = fs4::FileExt::unlock(lock);
            let _ = std::fs::remove_file(&self.path);
            let _ = fs4::FileExt::unlock(coordination);
        }
    }
}

pub trait BlobBackend: Send + Sync {
    fn capabilities(&self) -> BlobCapabilities;

    fn health(&self) -> impl Future<Output = Result<(), BlobError>> + Send;

    fn open(
        &self,
        digest: Digest,
        range: Option<Range<u64>>,
    ) -> impl Future<Output = Result<BlobRead, BlobError>> + Send;

    fn head(&self, digest: Digest) -> impl Future<Output = Result<Option<BlobMetadata>, BlobError>> + Send;

    fn begin(&self) -> impl Future<Output = Result<BlobWrite, BlobError>> + Send;

    fn verify(&self, digest: Digest) -> impl Future<Output = Result<bool, BlobError>> + Send;

    fn delete(&self, digest: Digest) -> impl Future<Output = Result<bool, BlobError>> + Send;

    fn materialize(&self, digest: Digest) -> impl Future<Output = Result<BlobLease, BlobError>> + Send;
}

impl BlobBackend for BlobStore {
    fn capabilities(&self) -> BlobCapabilities {
        BlobCapabilities {
            durability: BlobDurability::Filesystem,
            create_if_absent: BlobSupport::Native,
            range: BlobSupport::Native,
            checksum: BlobSupport::Emulated,
            delete: BlobSupport::Native,
            list: BlobSupport::Native,
            local_tail: BlobSupport::Native,
        }
    }

    async fn health(&self) -> Result<(), BlobError> {
        run_without_digest(self.clone(), BlobOperation::Health, |store| store.health_check()).await
    }

    async fn open(&self, digest: Digest, range: Option<Range<u64>>) -> Result<BlobRead, BlobError> {
        run(self.clone(), digest, BlobOperation::Open, move |store, digest| {
            let file = std::fs::File::open(store.path_for(&digest)).map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    BlobError::not_found(&digest)
                } else {
                    error.into()
                }
            })?;
            let bytes = file.metadata()?.len();
            let range = range.unwrap_or(0..bytes);
            if range.start > range.end || range.end > bytes {
                return Err(BlobError::invalid_range(range.start, range.end, bytes));
            }
            Ok(BlobRead::new(
                "filesystem",
                digest,
                BlobMetadata {
                    bytes,
                    modified: file.metadata()?.modified().ok(),
                },
                range,
                BlobReadBody::File(file),
            ))
        })
        .await
    }

    async fn head(&self, digest: Digest) -> Result<Option<BlobMetadata>, BlobError> {
        run(self.clone(), digest, BlobOperation::Head, |store, digest| {
            store.head(&digest)
        })
        .await
    }

    async fn begin(&self) -> Result<BlobWrite, BlobError> {
        run_without_digest(self.clone(), BlobOperation::Write, BlobWrite::filesystem).await
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

    async fn materialize(&self, digest: Digest) -> Result<BlobLease, BlobError> {
        run(self.clone(), digest, BlobOperation::Materialize, |store, digest| {
            let path = store.path_for(&digest);
            BlobLease::pinned(&path, &store.lease_dir()).map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    BlobError::not_found(&digest)
                } else {
                    error.into()
                }
            })
        })
        .await
    }
}

async fn run<T>(
    store: BlobStore,
    digest: Digest,
    operation: BlobOperation,
    action: impl FnOnce(BlobStore, Digest) -> Result<T, BlobError> + Send + 'static,
) -> Result<T, BlobError>
where
    T: Send + 'static,
{
    let permit = store.worker_permit().await;
    let error_digest = digest.clone();
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        action(store, digest)
    })
    .await
    .map_err(|error| BlobError::from(error).with_context("filesystem", operation, Some(&error_digest)))?
    .map_err(|error| error.with_context("filesystem", operation, Some(&error_digest)))
}

async fn run_without_digest<T>(
    store: BlobStore,
    operation: BlobOperation,
    action: impl FnOnce(BlobStore) -> Result<T, BlobError> + Send + 'static,
) -> Result<T, BlobError>
where
    T: Send + 'static,
{
    let permit = store.worker_permit().await;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        action(store)
    })
    .await
    .map_err(|error| BlobError::from(error).with_context("filesystem", operation, None))?
    .map_err(|error| error.with_context("filesystem", operation, None))
}

#[cfg(test)]
#[path = "../../tests/unit/blob/backend/s3_staged_tests.rs"]
mod s3_staged_tests;

#[cfg(test)]
#[path = "../../tests/unit/blob/backend/lease_tests.rs"]
mod lease_tests;
