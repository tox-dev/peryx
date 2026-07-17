use std::io::{Read as _, Seek as _, Write as _};
use std::ops::Range;
use std::path::{Component, Path, PathBuf};

use sha2::{Digest as _, Sha256};

use super::error::{BlobError, BlobScanError};
use super::{BlobMetadata, Digest, sync_parent, to_hex};

/// Settle a no-clobber move of a freshly written temp blob into its content-addressed `dest`.
///
/// A no-clobber rename fails when `dest` already exists on every platform (a plain rename overwrites on
/// Unix but errors on Windows, so two racing writers of the same digest would diverge). Since the bytes
/// are identical for a given digest, a `dest` that already holds it means the blob is stored, so a lost
/// race is success. Any other failure is a real io error.
fn commit_placement(persisted: Result<(), std::io::Error>, dest: &Path) -> Result<(), BlobError> {
    match persisted {
        Ok(()) => {
            sync_parent(dest);
            Ok(())
        }
        Err(_) if dest.is_file() => Ok(()),
        Err(err) => Err(err.into()),
    }
}

/// Name the blob a failed open was looking for. Opening already reports absence, so asking the
/// filesystem whether the path is a file beforehand only re-walks the same directories.
fn absent_or_io(err: std::io::Error, digest: &Digest) -> BlobError {
    if err.kind() == std::io::ErrorKind::NotFound {
        return BlobError::not_found(digest);
    }
    err.into()
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

    /// Write `bytes`, returning their digest. Idempotent: an existing blob is left untouched.
    ///
    /// # Errors
    /// Returns [`BlobError::Io`] if the directory cannot be created or the file cannot be written.
    pub fn write(&self, bytes: &[u8]) -> Result<Digest, BlobError> {
        let digest = Digest::of(bytes);
        let hex = digest.as_str();
        let parent = self.root.join("sha256").join(&hex[0..2]).join(&hex[2..4]);
        let dest = parent.join(hex);
        if dest.is_file() {
            return Ok(digest);
        }
        std::fs::create_dir_all(&parent)?;
        let mut tmp = tempfile::NamedTempFile::new_in(&parent)?;
        tmp.write_all(bytes)?;
        tmp.as_file().sync_all()?;
        commit_placement(tmp.persist_noclobber(&dest).map(drop).map_err(|err| err.error), &dest)?;
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
        let mut hasher = Sha256::new();
        let mut buffer = vec![0; 1024 * 1024].into_boxed_slice();
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        Ok(to_hex(&hasher.finalize()) == digest.as_str())
    }

    /// Remove a blob by digest, returning whether a file existed.
    ///
    /// # Errors
    /// Returns [`BlobError::Io`] if the filesystem removal fails.
    pub fn remove(&self, digest: &Digest) -> Result<bool, BlobError> {
        match std::fs::remove_file(self.path_for(digest)) {
            Ok(()) => Ok(true),
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
    /// Buffered so wheel-sized streams issue hundreds of large writes instead of one syscall per
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

    /// Move a staged blob into the store.
    ///
    /// # Errors
    /// Returns [`BlobError::Io`] on a filesystem failure.
    ///
    /// # Panics
    /// Never in practice: blob paths always sit inside the store root, so a parent exists.
    pub fn commit_staged(&self, staged: StagedBlob) -> Result<(), BlobError> {
        let dest = self.path_for(&staged.digest);
        if dest.is_file() {
            return Ok(());
        }
        std::fs::create_dir_all(dest.parent().expect("blob paths always have a parent"))?;
        commit_placement(staged.path.persist_noclobber(&dest).map_err(|err| err.error), &dest)
    }

    /// Finish a streamed write: verify the digest and move the blob into place.
    ///
    /// # Errors
    /// Returns [`BlobError::DigestMismatch`] when the streamed bytes hash differently, or
    /// [`BlobError::Io`] on a filesystem failure.
    ///
    /// # Panics
    /// Never in practice: blob paths always sit inside the store root, so a parent exists.
    pub fn commit(&self, pending: PendingBlob, expected: &Digest) -> Result<(), BlobError> {
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
        self.path.close()?;
        Ok(())
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
        self.path.close()?;
        Ok(())
    }
}

#[cfg(test)]
mod placement_tests {
    use super::commit_placement;
    #[test]
    fn test_commit_placement_succeeds_on_a_clean_move() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("blob");
        std::fs::write(&dest, b"bytes").unwrap();
        assert!(commit_placement(Ok(()), &dest).is_ok());
    }

    #[test]
    fn test_commit_placement_treats_a_lost_race_as_success() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("blob");
        std::fs::write(&dest, b"bytes").unwrap();
        let clash = std::io::Error::from(std::io::ErrorKind::AlreadyExists);
        assert!(commit_placement(Err(clash), &dest).is_ok());
    }

    #[test]
    fn test_commit_placement_reports_a_real_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("nothing-here");
        let failure = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert_eq!(
            commit_placement(Err(failure), &absent).unwrap_err().kind(),
            crate::blob::BlobErrorKind::Io
        );
    }
}
