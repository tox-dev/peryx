use std::future::Future;
use std::io::{Read as _, Seek as _};
use std::ops::Range;
use std::path::{Path, PathBuf};

use bytes::Bytes;
use futures_util::stream::BoxStream;
use futures_util::{StreamExt as _, TryStreamExt as _};

use super::error::{BlobError, BlobOperation};
use super::store::{BlobStore, PendingBlob, StagedBlob};
use super::{BlobMetadata, Digest};

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

/// The scope that acknowledges a successful write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobDurability {
    /// The selected filesystem acknowledged the write. Crash guarantees depend on that filesystem.
    Filesystem,
}

impl BlobDurability {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Filesystem => "filesystem",
        }
    }
}

/// How the selected backend provides an operation.
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

/// Effective operations and guarantees available through one configured backend.
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

/// A read result whose payload stays streamed.
pub struct BlobRead {
    pub metadata: BlobMetadata,
    pub range: Range<u64>,
    pub body: BlobReadBody,
    backend: &'static str,
    digest: Digest,
}

impl BlobRead {
    /// Build a backend result while retaining the context needed for deferred stream errors.
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
            BlobReadBody::Stream(stream) => BlobReadBody::Stream(checked_stream(
                stream,
                range.end.checked_sub(range.start),
                (range.start, range.end, metadata.bytes),
                backend,
                digest.clone(),
            )),
        };
        Self {
            metadata,
            range,
            body,
            backend,
            digest,
        }
    }

    /// Collect a result only when its declared size fits `max_bytes`.
    ///
    /// # Errors
    /// Returns a size or payload-read error.
    ///
    /// # Panics
    /// Panics if the internal blocking read task panics.
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
            .expect("blob collection task never panics"),
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
    expected: Option<u64>,
    declared: (u64, u64, u64),
    backend: &'static str,
    digest: Digest,
) -> BoxStream<'static, Result<Bytes, BlobError>> {
    futures_util::stream::try_unfold((stream, 0u64), move |(mut stream, received)| {
        let digest = digest.clone();
        async move {
            let Some(expected) = expected else {
                return Err(
                    BlobError::invalid_range(declared.0, declared.1, declared.2).with_context(
                        backend,
                        BlobOperation::Open,
                        Some(&digest),
                    ),
                );
            };
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

/// A concrete local file fast path or a backend-provided byte stream.
pub enum BlobReadBody {
    File(std::fs::File),
    Stream(BoxStream<'static, Result<Bytes, BlobError>>),
}

/// A streamed write whose concrete staging strategy is private to the backend facade.
pub struct BlobWrite {
    backend: BlobWriteBackend,
}

enum BlobWriteBackend {
    Filesystem(FilesystemWrite),
}

struct FilesystemWrite {
    store: BlobStore,
    pending: Option<PendingBlob>,
    task: Option<tokio::task::JoinHandle<Result<PendingBlob, BlobError>>>,
    queued: Vec<Bytes>,
    queued_bytes: usize,
    tail: BlobTail,
}

const WRITE_BATCH_BYTES: usize = 1024 * 1024;

/// A completed staged write with its computed address and length.
#[derive(Debug)]
pub struct BlobStaged {
    backend: Option<BlobStagedBackend>,
}

#[derive(Debug)]
enum BlobStagedBackend {
    Filesystem { store: BlobStore, staged: StagedBlob },
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

    /// Append a chunk without buffering the complete blob.
    ///
    /// # Errors
    /// Returns a contextual write error when the backend rejects the chunk.
    pub async fn write_chunk(&mut self, chunk: Bytes) -> Result<(), BlobError> {
        match &mut self.backend {
            BlobWriteBackend::Filesystem(write) => write.write_chunk(chunk).await,
        }
    }

    /// Make written bytes visible to local tail readers.
    ///
    /// # Errors
    /// Returns a contextual write error when the backend cannot flush the stage.
    pub async fn flush(&mut self) -> Result<u64, BlobError> {
        match &mut self.backend {
            BlobWriteBackend::Filesystem(write) => write.flush().await,
        }
    }

    /// A cloneable handle for readers following an in-progress local stage.
    #[must_use]
    pub fn tail(&self) -> Option<BlobTail> {
        match &self.backend {
            BlobWriteBackend::Filesystem(write) => Some(write.tail.clone()),
        }
    }

    /// Verify the completed stream and publish it atomically.
    ///
    /// # Errors
    /// Returns a contextual commit error on mismatch or storage failure.
    pub async fn commit(self, expected: &Digest) -> Result<(), BlobError> {
        self.finish().await?.commit_as(expected).await
    }

    /// Finish hashing and syncing the stage without publishing it.
    ///
    /// # Errors
    /// Returns a contextual write error when the stage cannot be flushed or synced.
    pub async fn finish(self) -> Result<BlobStaged, BlobError> {
        match self.backend {
            BlobWriteBackend::Filesystem(write) => write.finish().await,
        }
    }

    /// Wait for accepted writes and remove the unpublished stage.
    ///
    /// # Errors
    /// Returns a contextual write error when an accepted batch failed.
    pub async fn abort(self) -> Result<(), BlobError> {
        match self.backend {
            BlobWriteBackend::Filesystem(write) => write.abort().await,
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
        .expect("blob finish task never panics");
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
        .expect("blob abort task never panics")
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
        let pending = task.await.expect("blob batch task never panics");
        self.pending = Some(filesystem_context(pending, BlobOperation::Write, None)?);
        Ok(())
    }
}

impl Drop for FilesystemWrite {
    fn drop(&mut self) {
        let pending = self.pending.take();
        let task = self.task.take();
        let handle = tokio::runtime::Handle::try_current().ok();
        spawn_blocking_or_run(move || {
            // An accepted batch still owns the stage on a worker thread. Reclaim it so its file handle
            // is released before `abort` removes the stage: Windows refuses to unlink a file another
            // handle holds open.
            let pending = pending.or_else(move || handle?.block_on(task?).ok()?.ok());
            if let Some(pending) = pending {
                let _ = pending.abort();
            }
        });
    }
}

impl BlobStaged {
    pub(crate) const fn filesystem(store: BlobStore, staged: StagedBlob) -> Self {
        Self {
            backend: Some(BlobStagedBackend::Filesystem { store, staged }),
        }
    }

    #[must_use]
    pub const fn digest(&self) -> &Digest {
        match self.backend() {
            BlobStagedBackend::Filesystem { staged, .. } => staged.digest(),
        }
    }

    #[must_use]
    pub const fn len(&self) -> u64 {
        match self.backend() {
            BlobStagedBackend::Filesystem { staged, .. } => staged.len(),
        }
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        match self.backend() {
            BlobStagedBackend::Filesystem { staged, .. } => staged.is_empty(),
        }
    }

    /// Run seekable inspection while retaining ownership of the temporary stage.
    pub fn with_materialized<T>(&self, inspect: impl FnOnce(&Path) -> T) -> T {
        match self.backend() {
            BlobStagedBackend::Filesystem { staged, .. } => inspect(staged.path()),
        }
    }

    /// Publish the stage at its computed content address.
    ///
    /// # Errors
    /// Returns a contextual commit error on storage failure.
    ///
    /// # Panics
    /// Panics if the internal blocking task panics.
    pub async fn commit(mut self) -> Result<(), BlobError> {
        let permit = match self.backend() {
            BlobStagedBackend::Filesystem { store, .. } => store.worker_permit().await,
        };
        let backend = self.take_backend();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            Self::commit_backend(backend)
        })
        .await
        .expect("blob commit task never panics")
    }

    pub(crate) fn commit_blocking(mut self) -> Result<(), BlobError> {
        let backend = self.take_backend();
        Self::commit_backend(backend)
    }

    /// Publish only when the computed address matches `expected`.
    ///
    /// # Errors
    /// Returns a contextual digest mismatch or commit error.
    pub async fn commit_as(self, expected: &Digest) -> Result<(), BlobError> {
        if self.digest() != expected {
            let error = BlobError::digest_mismatch(expected, self.digest()).with_context(
                "filesystem",
                BlobOperation::Commit,
                Some(expected),
            );
            self.abort().await?;
            return Err(error);
        }
        self.commit().await
    }

    pub(crate) fn commit_as_blocking(self, expected: &Digest) -> Result<(), BlobError> {
        if self.digest() != expected {
            let error = BlobError::digest_mismatch(expected, self.digest()).with_context(
                "filesystem",
                BlobOperation::Commit,
                Some(expected),
            );
            self.abort_blocking()?;
            return Err(error);
        }
        self.commit_blocking()
    }

    /// Remove the unpublished stage.
    ///
    /// # Errors
    /// Returns a contextual cleanup error.
    ///
    /// # Panics
    /// Panics if the internal blocking task panics.
    pub async fn abort(mut self) -> Result<(), BlobError> {
        let permit = match self.backend() {
            BlobStagedBackend::Filesystem { store, .. } => store.worker_permit().await,
        };
        let backend = self.take_backend();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            Self::abort_backend(backend)
        })
        .await
        .expect("blob abort task never panics")
    }

    fn abort_blocking(mut self) -> Result<(), BlobError> {
        let backend = self.take_backend();
        Self::abort_backend(backend)
    }

    const fn backend(&self) -> &BlobStagedBackend {
        self.backend.as_ref().expect("staged blob retains its backend")
    }

    const fn take_backend(&mut self) -> BlobStagedBackend {
        self.backend.take().expect("staged blob retains its backend")
    }

    fn commit_backend(backend: BlobStagedBackend) -> Result<(), BlobError> {
        match backend {
            BlobStagedBackend::Filesystem { store, staged } => {
                let digest = staged.digest().clone();
                filesystem_context(store.commit_staged(staged), BlobOperation::Commit, Some(&digest))
            }
        }
    }

    fn abort_backend(backend: BlobStagedBackend) -> Result<(), BlobError> {
        match backend {
            BlobStagedBackend::Filesystem { staged, .. } => staged
                .abort()
                .map_err(|error| error.with_context("filesystem", BlobOperation::Write, None)),
        }
    }
}

impl Drop for BlobStaged {
    fn drop(&mut self) {
        if let Some(backend) = self.backend.take() {
            spawn_blocking_or_run(move || {
                let _ = Self::abort_backend(backend);
            });
        }
    }
}

fn spawn_blocking_or_run(action: impl FnOnce() + Send + 'static) {
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        drop(runtime.spawn_blocking(action));
    } else {
        action();
    }
}

/// Access to bytes already flushed by an in-progress local write.
#[derive(Clone, Debug)]
pub struct BlobTail {
    path: PathBuf,
}

impl BlobTail {
    /// Open the current stage from its beginning.
    ///
    /// # Errors
    /// Returns an I/O error if the stage has already moved or cannot be read.
    pub fn open(&self) -> std::io::Result<std::fs::File> {
        std::fs::File::open(&self.path)
    }
}

/// A seekable local view held for the lifetime of archive or backup work.
#[derive(Debug)]
pub struct BlobLease {
    path: PathBuf,
    lock: std::fs::File,
    coordination: std::fs::File,
    _temporary: tempfile::TempPath,
}

impl BlobLease {
    pub(crate) fn pinned(path: &Path, lease_dir: &Path) -> Result<Self, std::io::Error> {
        std::fs::create_dir_all(lease_dir)?;
        let coordination = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(lease_dir.join(".cleanup.lock"))?;
        fs4::fs_std::FileExt::lock_shared(&coordination)?;
        let mut source = std::fs::File::open(path)?;
        let temporary = tempfile::Builder::new()
            .prefix(".peryx-lease-")
            .tempfile_in(lease_dir)?
            .into_temp_path();
        std::fs::remove_file(&temporary)?;
        let lock = if std::fs::hard_link(path, &temporary).is_ok() {
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
        fs4::fs_std::FileExt::lock_shared(&lock)?;
        fs4::fs_std::FileExt::unlock(&coordination)?;
        Ok(Self {
            path: temporary.to_path_buf(),
            lock,
            coordination,
            _temporary: temporary,
        })
    }

    /// The materialized file. The path is valid only while this lease is alive.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for BlobLease {
    fn drop(&mut self) {
        let _ = fs4::fs_std::FileExt::lock_shared(&self.coordination);
        let _ = fs4::fs_std::FileExt::unlock(&self.lock);
        let _ = std::fs::remove_file(&self.path);
        let _ = fs4::fs_std::FileExt::unlock(&self.coordination);
    }
}

/// The backend-neutral operations used by protocol and maintenance code.
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
    .expect("blob backend task never panics")
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
    .expect("blob backend task never panics")
    .map_err(|error| error.with_context("filesystem", operation, None))
}
