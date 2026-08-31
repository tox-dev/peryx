use std::io::{ErrorKind, Read as _, Seek as _, SeekFrom, Write as _};
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use sha2::{Digest as _, Sha256};

use super::error::{BlobError, BlobScanError};
use super::stage::{OwnedPath, PathOwners, STAGE_MAX_AGE, STAGE_PREFIX, StageUsage, is_stage};
use super::{BlobMetadata, Digest, DurabilityCapabilities, PlacementReceipt, WriteEvidence, sync_parent};

/// An occupied digest path may contain corrupt bytes, so a failed no-clobber move verifies the resident
/// file before discarding the trusted source.
fn publish(dest: &Path, source: tempfile::TempPath, digest: &Digest, len: u64) -> Result<(), BlobError> {
    match source.persist_noclobber(dest) {
        Ok(()) => {
            sync_parent(dest);
            Ok(())
        }
        Err(err) => reconcile(dest, err.path, digest, len),
    }
}

/// Holds the digest lock until the resident file is verified or replaced, preventing concurrent repairs
/// from discarding the trusted source.
fn reconcile(dest: &Path, source: tempfile::TempPath, digest: &Digest, len: u64) -> Result<(), BlobError> {
    let _guard = digest_lock(digest);
    if resident_matches(dest, digest, len)? {
        return discard_stage(source);
    }
    source.persist(dest).map_err(|err| err.error)?;
    sync_parent(dest);
    Ok(())
}

/// Rejects a truncated resident before paying for a full hash.
fn resident_matches(dest: &Path, digest: &Digest, len: u64) -> Result<bool, BlobError> {
    let mut file = std::fs::File::open(dest)?;
    if file.metadata()?.len() != len {
        return Ok(false);
    }
    Ok(hash_file(&mut file)? == *digest)
}

/// Large reads avoid one syscall per hash block.
fn hash_file(file: &mut std::fs::File) -> std::io::Result<Digest> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 1024 * 1024].into_boxed_slice();
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(Digest::from_sha256(hasher.finalize().into()))
}

/// Serializes verify-and-replace work for the same digest.
fn digest_lock(digest: &Digest) -> std::sync::MutexGuard<'static, ()> {
    static LOCKS: [std::sync::Mutex<()>; 64] = [const { std::sync::Mutex::new(()) }; 64];
    let shard = digest
        .as_str()
        .bytes()
        .fold(0usize, |acc, byte| acc.wrapping_add(usize::from(byte)))
        % LOCKS.len();
    LOCKS[shard].lock().expect("blob digest lock is never poisoned")
}

/// Avoids a separate existence check and its second directory walk.
fn absent_or_io(err: std::io::Error, digest: &Digest) -> BlobError {
    if err.kind() == std::io::ErrorKind::NotFound {
        return BlobError::not_found(digest);
    }
    err.into()
}

/// Renaming first frees the stage name on Windows, where open handles leave an unlinked file in a
/// delete-pending state that rejects new readers with `PermissionDenied`.
fn discard_stage(path: tempfile::TempPath) -> Result<(), BlobError> {
    discard_stage_with(path, rename_stage, tempfile::TempPath::close)
}

fn rename_stage(from: &Path, to: &Path) -> Result<(), std::io::Error> {
    std::fs::rename(from, to)
}

fn discard_stage_with(
    path: tempfile::TempPath,
    rename: fn(&Path, &Path) -> Result<(), std::io::Error>,
    close: fn(tempfile::TempPath) -> Result<(), std::io::Error>,
) -> Result<(), BlobError> {
    let scratch = scratch_path(&path);
    if rename(&path, &scratch).is_err() {
        return close(path).map_err(BlobError::from);
    }
    remove_pending(&scratch)
}

fn scratch_path(path: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut name = path.file_name().unwrap_or_default().to_owned();
    name.push(format!(
        ".discard-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    path.with_file_name(name)
}

/// A failed directory removal ends pruning because another blob may still occupy that branch.
fn prune_empty_parents(path: &Path, stop_at: &Path) {
    let mut current = path.parent();
    while let Some(dir) = current {
        if dir == stop_at || std::fs::remove_dir(dir).is_err() {
            break;
        }
        current = dir.parent();
    }
}

fn remove_pending(path: &Path) -> Result<(), BlobError> {
    remove_pending_with(path, |path| std::fs::remove_file(path), std::thread::sleep)
}

fn remove_pending_with(
    path: &Path,
    mut remove: impl FnMut(&Path) -> Result<(), std::io::Error>,
    mut wait: impl FnMut(Duration),
) -> Result<(), BlobError> {
    let mut backoff = Duration::from_millis(1);
    loop {
        match remove(path) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
            Err(error) if error.kind() == ErrorKind::PermissionDenied && backoff < Duration::from_millis(64) => {
                wait(backoff);
                backoff *= 2;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn lease_lock_available(result: Result<(), fs4::TryLockError>) -> Result<bool, std::io::Error> {
    match result {
        Ok(()) => Ok(true),
        Err(fs4::TryLockError::WouldBlock) => Ok(false),
        Err(fs4::TryLockError::Error(error)) => Err(error),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobEntry {
    pub path: PathBuf,
    pub digest: Option<Digest>,
    pub bytes: u64,
}

#[derive(Debug, Clone)]
pub struct BlobStore {
    root: PathBuf,
    workers: std::sync::Arc<tokio::sync::Semaphore>,
    owners: std::sync::Arc<PathOwners>,
}

impl BlobStore {
    /// Defers directory creation until the first write or health check.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            workers: std::sync::Arc::new(tokio::sync::Semaphore::new(8)),
            owners: std::sync::Arc::default(),
        }
    }

    /// Keeps a sweep off `path` until the returned guard drops.
    pub(crate) fn own(&self, path: PathBuf) -> OwnedPath {
        self.owners.own(path)
    }

    pub(crate) fn owns(&self, path: &Path) -> bool {
        self.owners.owns(path)
    }

    pub(crate) async fn worker_permit(&self) -> tokio::sync::OwnedSemaphorePermit {
        self.workers
            .clone()
            .acquire_owned()
            .await
            .expect("the private blob worker semaphore is never closed")
    }

    #[must_use]
    pub fn path_for(&self, digest: &Digest) -> PathBuf {
        self.parent_for(digest).join(digest.as_str())
    }

    fn parent_for(&self, digest: &Digest) -> PathBuf {
        let hex = digest.as_str();
        self.root.join("sha256").join(&hex[0..2]).join(&hex[2..4])
    }

    fn create_path_for(&self, digest: &Digest) -> Result<PathBuf, BlobError> {
        let parent = self.parent_for(digest);
        std::fs::create_dir_all(&parent)?;
        Ok(parent.join(digest.as_str()))
    }

    pub(crate) fn lease_dir(&self) -> PathBuf {
        self.root.join(".leases")
    }

    pub(crate) fn staging_dir(&self) -> PathBuf {
        self.root.clone()
    }

    #[must_use]
    pub fn exists(&self, digest: &Digest) -> bool {
        self.path_for(digest).is_file()
    }

    /// # Errors
    /// Returns [`super::BlobErrorKind::Io`] when the root cannot be created or opened as a directory.
    pub fn health_check(&self) -> Result<(), BlobError> {
        std::fs::create_dir_all(&self.root)?;
        std::fs::read_dir(&self.root)?;
        self.cleanup_leases()?;
        Ok(())
    }

    fn cleanup_leases(&self) -> Result<(), BlobError> {
        let lease_dir = self.lease_dir();
        if !lease_dir.is_dir() {
            return Ok(());
        }
        let coordination = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(lease_dir.join(".cleanup.lock"))?;
        fs4::FileExt::lock(&coordination)?;
        for entry in std::fs::read_dir(lease_dir)? {
            let entry = entry?;
            if !entry.file_name().to_string_lossy().starts_with(".peryx-lease-") {
                continue;
            }
            let Some(file) = open_lease(&entry.path())? else {
                continue;
            };
            if lease_lock_available(fs4::FileExt::try_lock(&file))? {
                fs4::FileExt::unlock(&file)?;
                drop(file);
                std::fs::remove_file(entry.path())?;
            }
        }
        Ok(())
    }

    /// Reclaims the stage files a process that died without running destructors left behind. A stage a
    /// live write owns is kept, and so is one young enough to belong to a write another process on this
    /// store is still streaming. Run this before the store serves traffic.
    ///
    /// # Errors
    /// Returns [`super::BlobErrorKind::Io`] when the store cannot be walked.
    pub(crate) fn sweep_stages(&self) -> Result<usize, BlobError> {
        let now = std::time::SystemTime::now();
        let mut swept = 0;
        self.visit_stages(&mut |path, metadata| {
            if self.owns(path) || now.duration_since(metadata.modified()?).unwrap_or_default() < STAGE_MAX_AGE {
                return Ok(());
            }
            match std::fs::remove_file(path) {
                Ok(()) => swept += 1,
                // One stranded stage must not strand the rest; the next sweep retries it.
                Err(error) => tracing::warn!(%error, path = %path.display(), "retained an abandoned blob stage"),
            }
            Ok(())
        })?;
        Ok(swept)
    }

    /// # Errors
    /// Returns [`super::BlobErrorKind::Io`] when the store cannot be walked.
    pub(crate) fn stage_usage(&self) -> Result<StageUsage, BlobError> {
        let mut usage = StageUsage::default();
        self.visit_stages(&mut |_, metadata| {
            usage.files += 1;
            usage.bytes += metadata.len();
            Ok(())
        })?;
        Ok(usage)
    }

    /// Walks the whole store because a stage sits either in the root or in the fan-out directory its
    /// write was addressed to.
    fn visit_stages(
        &self,
        visit: &mut dyn FnMut(&Path, &std::fs::Metadata) -> Result<(), BlobError>,
    ) -> Result<(), BlobError> {
        let mut dirs = vec![self.root.clone()];
        while let Some(dir) = dirs.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            for entry in entries {
                let entry = entry?;
                let file_type = entry.file_type()?;
                if file_type.is_dir() {
                    dirs.push(entry.path());
                } else if file_type.is_file() && is_stage(&entry.file_name()) {
                    visit(&entry.path(), &entry.metadata()?)?;
                }
            }
        }
        Ok(())
    }

    /// Leaves a matching resident untouched and repairs a corrupt resident from `bytes`.
    ///
    /// # Errors
    /// Returns [`super::BlobErrorKind::Io`] if the directory cannot be created or the file cannot be written.
    pub fn write(&self, bytes: &[u8]) -> Result<Digest, BlobError> {
        let digest = Digest::of(bytes);
        let hex = digest.as_str();
        let parent = self.root.join("sha256").join(&hex[0..2]).join(&hex[2..4]);
        let dest = parent.join(hex);
        if dest.is_file() && resident_matches(&dest, &digest, bytes.len() as u64)? {
            return Ok(digest);
        }
        std::fs::create_dir_all(&parent)?;
        let mut tmp = stage_file(&parent)?;
        let _owned = self.own(tmp.path().to_owned());
        tmp.write_all(bytes)?;
        tmp.as_file().sync_all()?;
        publish(&dest, tmp.into_temp_path(), &digest, bytes.len() as u64)?;
        Ok(digest)
    }

    /// Commits only bytes that hash to `expected`.
    ///
    /// # Errors
    /// Returns [`super::BlobErrorKind::DigestMismatch`] if the bytes hash to a different digest, or
    /// [`super::BlobErrorKind::Io`] on a filesystem failure.
    pub fn write_verified(&self, bytes: &[u8], expected: &Digest) -> Result<(), BlobError> {
        let actual = Digest::of(bytes);
        if &actual != expected {
            return Err(BlobError::digest_mismatch(expected, &actual));
        }
        self.write(bytes)?;
        Ok(())
    }

    /// # Errors
    /// Returns [`super::BlobErrorKind::NotFound`] if the blob is absent, or [`super::BlobErrorKind::Io`] on a read
    /// failure.
    pub fn read(&self, digest: &Digest) -> Result<Vec<u8>, BlobError> {
        std::fs::read(self.path_for(digest)).map_err(|err| absent_or_io(err, digest))
    }

    /// Returns `None` when the blob is absent without reading its contents.
    ///
    /// # Errors
    /// Returns [`super::BlobErrorKind::Io`] if the path exists but its metadata cannot be read.
    pub fn head(&self, digest: &Digest) -> Result<Option<BlobMetadata>, BlobError> {
        match std::fs::metadata(self.path_for(digest)) {
            Ok(metadata) if metadata.is_file() => Ok(Some(BlobMetadata {
                bytes: metadata.len(),
                modified: metadata.modified().ok(),
            })),
            Ok(_) => Ok(None),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    /// Reads an end-exclusive byte range.
    ///
    /// # Errors
    /// Returns [`super::BlobErrorKind::NotFound`] if the blob is absent, [`super::BlobErrorKind::InvalidRange`] if the
    /// range lies outside the blob, or [`super::BlobErrorKind::Io`] on a read failure.
    pub fn read_range(&self, digest: &Digest, range: Range<u64>) -> Result<Vec<u8>, BlobError> {
        let mut file = std::fs::File::open(self.path_for(digest)).map_err(|err| absent_or_io(err, digest))?;
        let bytes = file.metadata()?.len();
        let invalid = || BlobError::invalid_range(range.start, range.end, bytes);
        let Some(range_len_u64) = range.end.checked_sub(range.start).filter(|_| range.end <= bytes) else {
            return Err(invalid());
        };
        #[cfg(target_pointer_width = "64")]
        let range_len = usize::try_from(range_len_u64).unwrap_or(usize::MAX);
        #[cfg(not(target_pointer_width = "64"))]
        let range_len = usize::try_from(range_len_u64).map_err(|_| invalid())?;
        file.seek(std::io::SeekFrom::Start(range.start))?;
        let mut result = vec![0; range_len];
        file.take(range_len as u64).read_exact(&mut result)?;
        Ok(result)
    }

    /// Invokes `visit` as entries are discovered instead of collecting them.
    ///
    /// # Errors
    /// Returns a scan error if directory walking fails or the visitor returns an error.
    pub fn scan<E>(&self, mut visit: impl FnMut(BlobEntry) -> Result<(), E>) -> Result<(), BlobScanError<E>> {
        let root = self.root.join("sha256");
        if !root.exists() {
            return Ok(());
        }
        let mut dirs = vec![root];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir).map_err(BlobError::from)? {
                let entry = entry.map_err(BlobError::from)?;
                let file_type = entry.file_type().map_err(BlobError::from)?;
                if file_type.is_dir() {
                    dirs.push(entry.path());
                } else if file_type.is_file() && !is_stage(&entry.file_name()) {
                    let path = entry.path();
                    visit(BlobEntry {
                        bytes: entry.metadata().map_err(BlobError::from)?.len(),
                        digest: self.digest_from_path(&path),
                        path,
                    })
                    .map_err(BlobScanError::Visit)?;
                }
            }
        }
        Ok(())
    }

    /// # Errors
    /// Returns [`super::BlobErrorKind::NotFound`] if the blob is absent, or [`super::BlobErrorKind::Io`] on a read
    /// failure.
    pub fn verify(&self, digest: &Digest) -> Result<bool, BlobError> {
        let mut file = std::fs::File::open(self.path_for(digest)).map_err(|err| absent_or_io(err, digest))?;
        Ok(hash_file(&mut file)? == *digest)
    }

    fn upload_dir(&self) -> PathBuf {
        self.root.join("uploads")
    }

    /// Syncs `chunk` before returning. Truncating to the committed `offset` makes replay after a crash
    /// idempotent even when the previous chunk reached disk before its offset was committed.
    ///
    /// `session` must be a generated ID containing one safe path component.
    ///
    /// # Errors
    /// Returns [`super::BlobErrorKind::InvalidRange`] if `offset` exceeds the stage length, or
    /// [`super::BlobErrorKind::Io`] if the stage directory or file cannot be created or written.
    pub fn stage_upload_chunk(&self, session: &str, offset: u64, chunk: &[u8]) -> Result<u64, BlobError> {
        std::fs::create_dir_all(self.upload_dir())?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(self.upload_dir().join(session))?;
        let bytes = file.metadata()?.len();
        if offset > bytes {
            return Err(BlobError::invalid_range(offset, offset, bytes));
        }
        file.set_len(offset)?;
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(chunk)?;
        file.sync_all()?;
        Ok(file.metadata()?.len())
    }

    /// Returns `None` when `session` has no stage.
    ///
    /// # Errors
    /// Returns [`super::BlobErrorKind::Io`] if the stage exists but its metadata cannot be read.
    pub fn staged_upload_len(&self, session: &str) -> Result<Option<u64>, BlobError> {
        match std::fs::metadata(self.upload_dir().join(session)) {
            Ok(metadata) => Ok(Some(metadata.len())),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Publishes only bytes that hash to `expected`; an existing verified blob deduplicates the stage.
    ///
    /// # Errors
    /// Returns [`super::BlobErrorKind::DigestMismatch`] when the staged bytes hash differently,
    /// [`super::BlobErrorKind::NotFound`] when no stage exists, or [`super::BlobErrorKind::Io`] on a filesystem failure.
    ///
    pub fn finish_upload(&self, session: &str, expected: &Digest) -> Result<(), BlobError> {
        let stage = self.upload_dir().join(session);
        let mut file = match std::fs::File::open(&stage) {
            Ok(file) => file,
            // A retry after a lost response succeeds when the published blob outlived its stage.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound && self.path_for(expected).is_file() => {
                return Ok(());
            }
            Err(err) => return Err(absent_or_io(err, expected)),
        };
        let len = file.metadata()?.len();
        let actual = hash_file(&mut file)?;
        if actual != *expected {
            return Err(BlobError::digest_mismatch(expected, &actual));
        }
        drop(file);
        let dest = self.create_path_for(expected)?;
        publish(&dest, tempfile::TempPath::try_from_path(&stage)?, expected, len)
    }

    /// Treats an absent stage as a successful discard.
    ///
    /// # Errors
    /// Returns [`super::BlobErrorKind::Io`] on a filesystem failure other than the stage being absent.
    pub fn discard_upload(&self, session: &str) -> Result<(), BlobError> {
        match std::fs::remove_file(self.upload_dir().join(session)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    /// Prunes empty fan-out directories but leaves branches occupied by concurrent writers.
    ///
    /// # Errors
    /// Returns [`super::BlobErrorKind::Io`] if the filesystem removal fails.
    pub fn remove(&self, digest: &Digest) -> Result<bool, BlobError> {
        let path = self.path_for(digest);
        match std::fs::remove_file(&path) {
            Ok(()) => {
                prune_empty_parents(&path, &self.root.join("sha256"));
                Ok(true)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err.into()),
        }
    }

    fn digest_from_path(&self, path: &Path) -> Option<Digest> {
        let mut components = path.strip_prefix(self.root.join("sha256")).ok()?.components();
        let (Some(Component::Normal(first)), Some(Component::Normal(second)), Some(Component::Normal(filename)), None) = (
            components.next(),
            components.next(),
            components.next(),
            components.next(),
        ) else {
            return None;
        };
        let digest = Digest::from_hex(filename.to_str()?)?;
        let bytes = digest.as_str().as_bytes();
        if first.as_encoded_bytes() != &bytes[..2] || second.as_encoded_bytes() != &bytes[2..4] {
            return None;
        }
        Some(digest)
    }
}

/// Names every unpublished temporary alike so a sweep can recognize an abandoned one.
pub fn stage_file(directory: &Path) -> Result<tempfile::NamedTempFile, std::io::Error> {
    tempfile::Builder::new().prefix(STAGE_PREFIX).tempfile_in(directory)
}

fn open_lease(path: &Path) -> Result<Option<std::fs::File>, BlobError> {
    match std::fs::File::open(path) {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Keeps streamed bytes unpublished until their digest has been verified.
pub struct PendingBlob {
    /// Buffering avoids one syscall per network chunk.
    file: std::io::BufWriter<std::fs::File>,
    path: tempfile::TempPath,
    owned: OwnedPath,
    hasher: Sha256,
    len: u64,
}

#[derive(Debug)]
pub struct StagedBlob {
    path: tempfile::TempPath,
    /// Held until the stage is published or discarded, so a concurrent sweep leaves it alone.
    _owned: OwnedPath,
    digest: Digest,
    len: u64,
}

impl BlobStore {
    /// # Errors
    /// Returns [`super::BlobErrorKind::Io`] if the store directory or temp file cannot be created.
    pub fn begin(&self) -> Result<PendingBlob, BlobError> {
        std::fs::create_dir_all(&self.root)?;
        let (file, path) = stage_file(&self.root)?.into_parts();
        Ok(PendingBlob {
            file: std::io::BufWriter::with_capacity(1 << 20, file),
            owned: self.own(path.to_path_buf()),
            path,
            hasher: Sha256::new(),
            len: 0,
        })
    }

    /// Crosses the durability boundary by atomically publishing the synced stage and syncing its parent.
    /// A matching resident yields a receipt without a rewrite; a corrupt resident is replaced.
    ///
    /// # Errors
    /// Returns [`super::BlobErrorKind::Io`] on a filesystem failure.
    ///
    pub fn commit_staged(&self, staged: StagedBlob) -> Result<PlacementReceipt, BlobError> {
        let receipt = PlacementReceipt {
            digest: staged.digest.clone(),
            size: staged.len,
            durability: DurabilityCapabilities::FILESYSTEM,
            evidence: WriteEvidence::NodeLocal,
        };
        let dest = self.create_path_for(&staged.digest)?;
        publish(&dest, staged.path, &staged.digest, staged.len)?;
        Ok(receipt)
    }

    /// Publishes only when the streamed bytes hash to `expected`.
    ///
    /// # Errors
    /// Returns [`super::BlobErrorKind::DigestMismatch`] when the streamed bytes hash differently, or
    /// [`super::BlobErrorKind::Io`] on a filesystem failure.
    pub fn commit(&self, pending: PendingBlob, expected: &Digest) -> Result<PlacementReceipt, BlobError> {
        let staged = pending.finish()?;
        if staged.digest() != expected {
            return Err(BlobError::digest_mismatch(expected, staged.digest()));
        }
        self.commit_staged(staged)
    }
}

impl PendingBlob {
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.len
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// # Errors
    /// Returns [`super::BlobErrorKind::Io`] if the write fails.
    pub fn write(&mut self, chunk: &[u8]) -> Result<(), BlobError> {
        // Hash after the write so a partial write cannot produce a valid commit digest.
        self.file.write_all(chunk)?;
        self.hasher.update(chunk);
        self.len += chunk.len() as u64;
        Ok(())
    }

    /// Makes buffered bytes visible to readers tailing the temporary file.
    ///
    /// # Errors
    /// Returns [`super::BlobErrorKind::Io`] if the flush fails.
    pub fn flush(&mut self) -> Result<(), BlobError> {
        self.file.flush()?;
        Ok(())
    }

    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// # Errors
    /// Returns [`super::BlobErrorKind::Io`] if flushing or syncing the temporary file fails.
    pub fn finish(self) -> Result<StagedBlob, BlobError> {
        let file = self.file.into_inner().map_err(std::io::IntoInnerError::into_error)?;
        file.sync_all()?;
        Ok(StagedBlob {
            path: self.path,
            _owned: self.owned,
            digest: Digest::from_sha256(self.hasher.finalize().into()),
            len: self.len,
        })
    }

    pub(crate) fn abort(self) -> Result<(), BlobError> {
        let (file, _) = self.file.into_parts();
        drop(file);
        discard_stage(self.path)
    }
}

impl StagedBlob {
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    #[must_use]
    pub const fn digest(&self) -> &Digest {
        &self.digest
    }

    #[must_use]
    pub const fn len(&self) -> u64 {
        self.len
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub(crate) fn abort(self) -> Result<(), BlobError> {
        discard_stage(self.path)
    }
}

#[cfg(test)]
#[path = "../../tests/unit/blob/store/stage_tests.rs"]
mod stage_tests;

#[cfg(test)]
#[path = "../../tests/unit/blob/store/lease_tests.rs"]
mod lease_tests;
