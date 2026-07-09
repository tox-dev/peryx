//! The content-addressed blob store.
//!
//! A blob is stored once, keyed by the sha256 of its bytes, under a two-level hex fan-out
//! (`sha256/ab/cd/<digest>`). Writes go to a temp file in the destination directory, are fsynced,
//! and atomically renamed into place, so a blob is never visible until it is complete. The path is
//! the digest, so anything present is by construction correct.

use std::path::Path;

use sha2::{Digest as _, Sha256};

mod backend;
mod error;
mod store;

pub use backend::BlobBackend;
pub use error::{BlobError, BlobScanError};
pub use store::{BlobEntry, BlobStore, PendingBlob, StagedBlob};

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

/// fsync the directory a blob was renamed into, so the rename itself survives a crash even though the
/// data file was already fsynced. Failing to open or sync the directory is not fatal to the write, so
/// it is ignored rather than surfaced.
fn sync_parent(path: &Path) {
    if let Some(parent) = path.parent()
        && let Ok(directory) = std::fs::File::open(parent)
    {
        let _ = directory.sync_all();
    }
}
