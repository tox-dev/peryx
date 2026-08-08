use std::io::{ErrorKind, Read as _, Seek as _, SeekFrom, Write as _};
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use sha2::{Digest as _, Sha256};

use super::error::{BlobError, BlobScanError};
use super::{BlobMetadata, Digest, DurabilityCapabilities, PlacementReceipt, sync_parent, to_hex};

/// Move a verified `source` into its content-addressed `dest`, guaranteeing the published file's bytes
/// hash to `digest`. `source` was hashed into `digest` as it was written, so it is the trusted copy.
///
/// A no-clobber move that finds `dest` free settles the durability boundary and returns. A move that
/// loses to an occupied `dest` proves nothing about the resident file: the digest path may hold a
/// truncated or corrupted blob that a plain existence check would wrongly accept, blocking self-repair.
/// The occupant is therefore verified and, when it fails, replaced from `source`. Any other move
/// failure is a real io error.
fn publish(dest: &Path, source: tempfile::TempPath, digest: &Digest, len: u64) -> Result<(), BlobError> {
    match source.persist_noclobber(dest) {
        Ok(()) => {
            sync_parent(dest);
            Ok(())
        }
        Err(err) if dest.is_file() => reconcile(dest, err.path, digest, len),
        Err(err) => Err(err.error.into()),
    }
}

/// Reconcile a commit whose content-addressed `dest` is already occupied. Under a per-digest lock,
/// stream-hash the resident file: a matching size and hash means the blob is already durable, so the
/// redundant `source` is discarded. A mismatch means a corrupt blob squats the digest path - replace it
/// atomically with the verified `source` so a later read returns the blob rather than the damage. The
/// lock keeps a second writer of the same digest from racing the replacement, and the correct `source`
/// is never dropped until the resident file has been validated.
fn reconcile(dest: &Path, source: tempfile::TempPath, digest: &Digest, len: u64) -> Result<(), BlobError> {
    let _guard = digest_lock(digest);
    if resident_matches(dest, digest, len)? {
        return discard_stage(source);
    }
    source.persist(dest).map_err(|err| err.error)?;
    sync_parent(dest);
    Ok(())
}

/// Whether the file at `dest` is exactly the blob `digest` names: it must be `len` bytes and stream-hash
/// to `digest`. The length check is a cheap truncation reject before the full re-hash.
fn resident_matches(dest: &Path, digest: &Digest, len: u64) -> Result<bool, BlobError> {
    let mut file = std::fs::File::open(dest)?;
    if file.metadata()?.len() != len {
        return Ok(false);
    }
    Ok(hash_file(&mut file)? == digest.as_str())
}

/// Stream `file` through SHA-256, returning its hex digest. Buffered so a large blob issues a handful of
/// big reads instead of one syscall per block.
fn hash_file(file: &mut std::fs::File) -> std::io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 1024 * 1024].into_boxed_slice();
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(to_hex(&hasher.finalize()))
}

/// A striped lock serializing repairs of the same digest. Two writers publishing identical bytes never
/// interleave a verify-then-replace, so a corrupt resident file is replaced once rather than in a race.
fn digest_lock(digest: &Digest) -> std::sync::MutexGuard<'static, ()> {
    static LOCKS: [std::sync::Mutex<()>; 64] = [const { std::sync::Mutex::new(()) }; 64];
    let shard = digest
        .as_str()
        .bytes()
        .fold(0usize, |acc, byte| acc.wrapping_add(usize::from(byte)))
        % LOCKS.len();
    LOCKS[shard].lock().expect("blob digest lock is never poisoned")
}

/// Name the blob a failed open was looking for. Opening already reports absence, so asking the
/// filesystem whether the path is a file beforehand only re-walks the same directories.
fn absent_or_io(err: std::io::Error, digest: &Digest) -> BlobError {
    if err.kind() == std::io::ErrorKind::NotFound {
        return BlobError::not_found(digest);
    }
    err.into()
}

/// Drop an abandoned stage so a reader tailing it never faults on a half-deleted name.
///
/// On Windows an unlink only flags the file for deletion while any handle stays open - a follower
/// tailing the stage, or a reader the caller just closed - and the original name lingers in a
/// delete-pending state that answers openers with `PermissionDenied` until that handle releases.
/// Renaming the stage aside frees its name at once (a rename tolerates open handles), so a tail sees
/// it vanish instead; the moved file is then removed, retried briefly while a straggling handle lets
/// go. Unix unlinks immediately, so the rename is a harmless extra step there.
fn discard_stage(path: tempfile::TempPath) -> Result<(), BlobError> {
    let scratch = scratch_path(&path);
    if std::fs::rename(&path, &scratch).is_err() {
        return path.close().map_err(BlobError::from);
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

/// Walk up from a removed blob's path, dropping each now-empty fan-out directory until reaching
/// `stop_at` (the `sha256` root, left in place) or a directory another blob still occupies. A
/// non-empty directory makes `remove_dir` fail, which ends the walk without disturbing it.
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
    remove_pending_with(path, std::thread::sleep)
}

/// Delete a pending stage, retrying a transient `PermissionDenied` (a straggling handle on Windows)
/// with a doubling backoff up to 64ms. `wait` receives each backoff before the retry; production
/// sleeps, and a test records the schedule so it neither sleeps nor races a real clock.
fn remove_pending_with(path: &Path, mut wait: impl FnMut(Duration)) -> Result<(), BlobError> {
    let mut backoff = Duration::from_millis(1);
    loop {
        match std::fs::remove_file(path) {
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

/// A file found while walking the content-addressed blob tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobEntry {
    pub path: PathBuf,
    pub digest: Option<Digest>,
    pub bytes: u64,
}

/// A content-addressed blob store rooted at a directory.
#[derive(Debug, Clone)]
pub struct BlobStore {
    root: PathBuf,
    workers: std::sync::Arc<tokio::sync::Semaphore>,
}

impl BlobStore {
    /// Create a store rooted at `root`. The directory is created lazily on first write.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            workers: std::sync::Arc::new(tokio::sync::Semaphore::new(8)),
        }
    }

    pub(crate) async fn worker_permit(&self) -> tokio::sync::OwnedSemaphorePermit {
        self.workers
            .clone()
            .acquire_owned()
            .await
            .expect("the private blob worker semaphore is never closed")
    }

    /// The on-disk path a digest maps to.
    #[must_use]
    pub fn path_for(&self, digest: &Digest) -> PathBuf {
        let hex = digest.as_str();
        self.root.join("sha256").join(&hex[0..2]).join(&hex[2..4]).join(hex)
    }

    pub(crate) fn lease_dir(&self) -> PathBuf {
        self.root.join(".leases")
    }

    pub(crate) fn staging_dir(&self) -> PathBuf {
        self.root.clone()
    }

    /// Whether the blob is present.
    #[must_use]
    pub fn exists(&self, digest: &Digest) -> bool {
        self.path_for(digest).is_file()
    }

    /// Ensure the store root exists and can be read.
    ///
    /// # Errors
    /// Returns [`BlobError::Io`] when the root cannot be created or opened as a directory.
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
        fs4::fs_std::FileExt::lock_exclusive(&coordination)?;
        for entry in std::fs::read_dir(lease_dir)? {
            let entry = entry?;
            if !entry.file_name().to_string_lossy().starts_with(".peryx-lease-") {
                continue;
            }
            let file = match std::fs::File::open(entry.path()) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            if fs4::fs_std::FileExt::try_lock_exclusive(&file)? {
                fs4::fs_std::FileExt::unlock(&file)?;
                drop(file);
                std::fs::remove_file(entry.path())?;
            }
        }
        Ok(())
    }

    /// Write `bytes`, returning their digest. Idempotent: an existing blob that still matches the digest
    /// is left untouched; one that has been truncated or corrupted is repaired from `bytes`.
    ///
    /// # Errors
    /// Returns [`BlobError::Io`] if the directory cannot be created or the file cannot be written.
    pub fn write(&self, bytes: &[u8]) -> Result<Digest, BlobError> {
        let digest = Digest::of(bytes);
        let hex = digest.as_str();
        let parent = self.root.join("sha256").join(&hex[0..2]).join(&hex[2..4]);
        let dest = parent.join(hex);
        if dest.is_file() && resident_matches(&dest, &digest, bytes.len() as u64)? {
            return Ok(digest);
        }
        std::fs::create_dir_all(&parent)?;
        let mut tmp = tempfile::NamedTempFile::new_in(&parent)?;
        tmp.write_all(bytes)?;
        tmp.as_file().sync_all()?;
        publish(&dest, tmp.into_temp_path(), &digest, bytes.len() as u64)?;
        Ok(digest)
    }

    /// Write `bytes` only if they match `expected` (hash-verify-before-commit).
    ///
    /// # Errors
    /// Returns [`BlobError::DigestMismatch`] if the bytes hash to a different digest, or
    /// [`BlobError::Io`] on a filesystem failure.
    pub fn write_verified(&self, bytes: &[u8], expected: &Digest) -> Result<(), BlobError> {
        let actual = Digest::of(bytes);
        if &actual != expected {
            return Err(BlobError::digest_mismatch(expected, &actual));
        }
        self.write(bytes)?;
        Ok(())
    }

    /// Read a blob's bytes.
    ///
    /// # Errors
    /// Returns [`BlobError::NotFound`] if the blob is absent, or [`BlobError::Io`] on a read
    /// failure.
    pub fn read(&self, digest: &Digest) -> Result<Vec<u8>, BlobError> {
        std::fs::read(self.path_for(digest)).map_err(|err| absent_or_io(err, digest))
    }

    /// Return a blob's byte length without reading its contents, or `None` when it is absent.
    ///
    /// # Errors
    /// Returns [`BlobError::Io`] if the path exists but its metadata cannot be read.
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

    /// Read an end-exclusive byte range from a blob.
    ///
    /// # Errors
    /// Returns [`BlobError::NotFound`] if the blob is absent, [`BlobError::InvalidRange`] if the
    /// range lies outside the blob, or [`BlobError::Io`] on a read failure.
    pub fn read_range(&self, digest: &Digest, range: Range<u64>) -> Result<Vec<u8>, BlobError> {
        let mut file = std::fs::File::open(self.path_for(digest)).map_err(|err| absent_or_io(err, digest))?;
        let bytes = file.metadata()?.len();
        let invalid = || BlobError::invalid_range(range.start, range.end, bytes);
        if range.start > range.end || range.end > bytes {
            return Err(invalid());
        }
        #[cfg(target_pointer_width = "64")]
        let range_len = usize::try_from(range.end - range.start).unwrap_or(usize::MAX);
        #[cfg(not(target_pointer_width = "64"))]
        let range_len = usize::try_from(range.end - range.start).map_err(|_| invalid())?;
        file.seek(std::io::SeekFrom::Start(range.start))?;
        let mut result = vec![0; range_len];
        file.take(range_len as u64).read_exact(&mut result)?;
        Ok(result)
    }

    /// Visit blob files under the content-addressed tree without collecting the store.
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
                } else if file_type.is_file() {
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

    /// Stream-hash a stored blob and check that its bytes match its address.
    ///
    /// # Errors
    /// Returns [`BlobError::NotFound`] if the blob is absent, or [`BlobError::Io`] on a read
    /// failure.
    pub fn verify(&self, digest: &Digest) -> Result<bool, BlobError> {
        let mut file = std::fs::File::open(self.path_for(digest)).map_err(|err| absent_or_io(err, digest))?;
        Ok(hash_file(&mut file)? == digest.as_str())
    }

    /// The directory holding durable per-session upload stages, one file per in-progress session.
    fn upload_dir(&self) -> PathBuf {
        self.root.join("uploads")
    }

    /// Write `chunk` into `session`'s durable stage at `offset`, creating the stage if absent, returning
    /// the new staged length. The bytes are synced before returning, so an accepted chunk survives a
    /// restart and a resumed upload continues from this length.
    ///
    /// `offset` is the last committed length: the stage is truncated to it before writing, so a chunk
    /// that synced to disk before its offset was committed and was then lost to a restart is dropped and
    /// the re-sent chunk lands exactly where the client resumes. Resume is therefore idempotent - the
    /// stage is always the committed prefix plus this chunk, never a duplicated region.
    ///
    /// `session` must be a single safe path component; the caller supplies a generated session id.
    ///
    /// # Errors
    /// Returns [`BlobError::Io`] if the stage directory or file cannot be created or written.
    pub fn stage_upload_chunk(&self, session: &str, offset: u64, chunk: &[u8]) -> Result<u64, BlobError> {
        std::fs::create_dir_all(self.upload_dir())?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(self.upload_dir().join(session))?;
        file.set_len(offset)?;
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(chunk)?;
        file.sync_all()?;
        Ok(file.metadata()?.len())
    }

    /// The bytes staged for `session` so far, or `None` when it has no stage.
    ///
    /// # Errors
    /// Returns [`BlobError::Io`] if the stage exists but its metadata cannot be read.
    pub fn staged_upload_len(&self, session: &str) -> Result<Option<u64>, BlobError> {
        match std::fs::metadata(self.upload_dir().join(session)) {
            Ok(metadata) => Ok(Some(metadata.len())),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Verify `session`'s staged bytes hash to `expected`, publish them into the content store, and
    /// remove the stage. A staged digest already present is deduplicated and the stage removed.
    ///
    /// # Errors
    /// Returns [`BlobError::DigestMismatch`] when the staged bytes hash differently,
    /// [`BlobError::NotFound`] when no stage exists, or [`BlobError::Io`] on a filesystem failure.
    ///
    /// # Panics
    /// Never in practice: blob paths always sit inside the store root, so a parent exists.
    pub fn finish_upload(&self, session: &str, expected: &Digest) -> Result<(), BlobError> {
        let stage = self.upload_dir().join(session);
        let mut file = match std::fs::File::open(&stage) {
            Ok(file) => file,
            // A finish that already published this digest and cleared its stage is idempotent: a retry
            // whose response was lost finds the blob durably present and succeeds, the way commit_staged
            // does, rather than reporting the missing stage as a failed upload.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound && self.path_for(expected).is_file() => {
                return Ok(());
            }
            Err(err) => return Err(absent_or_io(err, expected)),
        };
        let len = file.metadata()?.len();
        let actual_hex = hash_file(&mut file)?;
        if actual_hex != expected.as_str() {
            let actual = Digest::from_hex(&actual_hex).expect("a sha-256 hex digest is valid");
            return Err(BlobError::digest_mismatch(expected, &actual));
        }
        drop(file);
        let dest = self.path_for(expected);
        std::fs::create_dir_all(dest.parent().expect("blob paths always have a parent"))?;
        publish(&dest, tempfile::TempPath::try_from_path(&stage)?, expected, len)
    }

    /// Discard `session`'s durable stage, tolerating one that is already gone.
    ///
    /// # Errors
    /// Returns [`BlobError::Io`] on a filesystem failure other than the stage being absent.
    pub fn discard_upload(&self, session: &str) -> Result<(), BlobError> {
        match std::fs::remove_file(self.upload_dir().join(session)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    /// Remove a blob by digest, returning whether a file existed.
    ///
    /// After removing the file, the two fan-out directories it hung under (`sha256/ab/cd` then
    /// `sha256/ab`) are pruned when they fall empty, so reclaiming a store's last blob under a prefix
    /// leaves no empty skeleton behind. A directory a concurrent writer still needs stays put, since the
    /// prune skips a non-empty directory and the write path recreates any it needs.
    ///
    /// # Errors
    /// Returns [`BlobError::Io`] if the filesystem removal fails.
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
        let mut components = path.strip_prefix(&self.root).ok()?.components();
        let (
            Some(Component::Normal(algorithm)),
            Some(Component::Normal(first)),
            Some(Component::Normal(second)),
            Some(Component::Normal(filename)),
            None,
        ) = (
            components.next(),
            components.next(),
            components.next(),
            components.next(),
            components.next(),
        )
        else {
            return None;
        };
        let first = first.as_encoded_bytes();
        let second = second.as_encoded_bytes();
        let filename_bytes = filename.as_encoded_bytes();
        if algorithm != std::ffi::OsStr::new("sha256")
            || first.len() != 2
            || second.len() != 2
            || filename_bytes.len() < 4
            || &filename_bytes[..2] != first
            || &filename_bytes[2..4] != second
        {
            return None;
        }
        Digest::from_hex(filename.to_str()?)
    }
}

/// An in-progress blob write: bytes stream into a temp file while the digest accumulates; on
/// success the file moves into the store only when the hash matches.
pub struct PendingBlob {
    /// Buffered so large artifact streams issue hundreds of writes instead of one syscall per
    /// network chunk.
    file: std::io::BufWriter<std::fs::File>,
    path: tempfile::TempPath,
    hasher: Sha256,
    len: u64,
}

/// A fully written temporary blob, ready to move into the content-addressed tree.
#[derive(Debug)]
pub struct StagedBlob {
    path: tempfile::TempPath,
    digest: Digest,
    len: u64,
}

impl BlobStore {
    /// Begin streaming a blob into the store.
    ///
    /// # Errors
    /// Returns [`BlobError::Io`] if the store directory or temp file cannot be created.
    pub fn begin(&self) -> Result<PendingBlob, BlobError> {
        std::fs::create_dir_all(&self.root)?;
        let temp = tempfile::NamedTempFile::new_in(&self.root)?;
        let (file, path) = temp.into_parts();
        Ok(PendingBlob {
            file: std::io::BufWriter::with_capacity(1 << 20, file),
            path,
            hasher: Sha256::new(),
            len: 0,
        })
    }

    /// Move a staged blob into the store, returning a [`PlacementReceipt`] proving it is durable.
    ///
    /// The staged file was already synced when it was finished; this crosses the rest of the durability
    /// boundary by atomically renaming it into its content-addressed path and syncing the parent
    /// directory. An already-present blob that still matches the digest is durable too and yields a
    /// receipt without a rewrite; a corrupt one squatting the path is repaired from the stage.
    ///
    /// # Errors
    /// Returns [`BlobError::Io`] on a filesystem failure.
    ///
    /// # Panics
    /// Never in practice: blob paths always sit inside the store root, so a parent exists.
    pub fn commit_staged(&self, staged: StagedBlob) -> Result<PlacementReceipt, BlobError> {
        let receipt = PlacementReceipt {
            digest: staged.digest.clone(),
            size: staged.len,
            durability: DurabilityCapabilities::FILESYSTEM,
        };
        let dest = self.path_for(&staged.digest);
        std::fs::create_dir_all(dest.parent().expect("blob paths always have a parent"))?;
        publish(&dest, staged.path, &staged.digest, staged.len)?;
        Ok(receipt)
    }

    /// Finish a streamed write: verify the digest, move the blob into place, and return its
    /// [`PlacementReceipt`].
    ///
    /// # Errors
    /// Returns [`BlobError::DigestMismatch`] when the streamed bytes hash differently, or
    /// [`BlobError::Io`] on a filesystem failure.
    ///
    /// # Panics
    /// Never in practice: blob paths always sit inside the store root, so a parent exists.
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

    /// Append one chunk.
    ///
    /// # Errors
    /// Returns [`BlobError::Io`] if the write fails.
    pub fn write(&mut self, chunk: &[u8]) -> Result<(), BlobError> {
        // Hash only what was written: a failed write leaves the digest short, so commit refuses
        // the incomplete blob instead of persisting it.
        self.file.write_all(chunk)?;
        self.hasher.update(chunk);
        self.len += chunk.len() as u64;
        Ok(())
    }

    /// Push buffered bytes to the file so readers tailing the temp path see them.
    ///
    /// # Errors
    /// Returns [`BlobError::Io`] if the flush fails.
    pub fn flush(&mut self) -> Result<(), BlobError> {
        self.file.flush()?;
        Ok(())
    }

    /// Where the in-progress bytes live until [`BlobStore::commit`] moves them into place.
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Finish writing and return the staged blob.
    ///
    /// # Errors
    /// Returns [`BlobError::Io`] if flushing or syncing the temporary file fails.
    pub fn finish(self) -> Result<StagedBlob, BlobError> {
        let file = self.file.into_inner().map_err(std::io::IntoInnerError::into_error)?;
        file.sync_all()?;
        Ok(StagedBlob {
            path: self.path,
            digest: Digest(to_hex(&self.hasher.finalize())),
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
    /// The staged file path.
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// The staged file digest.
    #[must_use]
    pub const fn digest(&self) -> &Digest {
        &self.digest
    }

    /// The staged byte length.
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.len
    }

    /// Whether the staged file has no bytes.
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
