//! The content-addressed blob store.
//!
//! A blob is stored once, keyed by the sha256 of its bytes, under a two-level hex fan-out
//! (`sha256/ab/cd/<digest>`). Writes go to a temp file in the destination directory, are fsynced,
//! and atomically renamed into place, so a blob is never visible until it is complete. The path is
//! the digest, so anything present is by construction correct.

use std::io::Write as _;
use std::path::PathBuf;

use sha2::{Digest as _, Sha256};

/// A sha256 digest rendered as lowercase hex.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Digest(String);

impl Digest {
    /// Compute the digest of `bytes`.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self(to_hex(&Sha256::digest(bytes)))
    }

    /// The digest as lowercase hex.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Parse a 64-character lowercase-hex sha256 digest, rejecting anything else.
    #[must_use]
    pub fn from_hex(hex: &str) -> Option<Self> {
        if hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            Some(Self(hex.to_owned()))
        } else {
            None
        }
    }
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// An error from the blob store.
#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    #[error("blob store io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("blob {0} not found")]
    NotFound(String),
    #[error("digest mismatch: expected {expected}, got {actual}")]
    DigestMismatch { expected: String, actual: String },
}

/// A content-addressed blob store rooted at a directory.
#[derive(Debug, Clone)]
pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    /// Create a store rooted at `root`. The directory is created lazily on first write.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The on-disk path a digest maps to.
    #[must_use]
    pub fn path_for(&self, digest: &Digest) -> PathBuf {
        let hex = digest.as_str();
        self.root.join("sha256").join(&hex[0..2]).join(&hex[2..4]).join(hex)
    }

    /// Whether the blob is present.
    #[must_use]
    pub fn exists(&self, digest: &Digest) -> bool {
        self.path_for(digest).is_file()
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
        tmp.persist(&dest).map_err(|err| err.error)?;
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
            return Err(BlobError::DigestMismatch {
                expected: expected.as_str().to_owned(),
                actual: actual.0,
            });
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
        let path = self.path_for(digest);
        if !path.is_file() {
            return Err(BlobError::NotFound(digest.as_str().to_owned()));
        }
        Ok(std::fs::read(&path)?)
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
        })
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
        let actual = Digest(to_hex(&pending.hasher.finalize()));
        if &actual != expected {
            return Err(BlobError::DigestMismatch {
                expected: expected.as_str().to_owned(),
                actual: actual.0,
            });
        }
        let file = pending.file.into_inner().map_err(std::io::IntoInnerError::into_error)?;
        file.sync_all()?;
        let dest = self.path_for(expected);
        std::fs::create_dir_all(dest.parent().expect("blob paths always have a parent"))?;
        pending.path.persist(&dest).map_err(|err| BlobError::Io(err.error))?;
        Ok(())
    }
}

impl PendingBlob {
    /// Append one chunk.
    ///
    /// # Errors
    /// Returns [`BlobError::Io`] if the write fails.
    pub fn write(&mut self, chunk: &[u8]) -> Result<(), BlobError> {
        // Hash only what was written: a failed write leaves the digest short, so commit refuses
        // the incomplete blob instead of persisting it.
        self.file.write_all(chunk)?;
        self.hasher.update(chunk);
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
}
