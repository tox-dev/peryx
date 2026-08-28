//! Chunk digests let ranged reads verify and forward each span before the full blob arrives. Staging
//! records them only after verifying the whole blob, so they derive from the bytes that produced the
//! content address.

use std::num::NonZeroU64;
use std::ops::Range;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::{Digest, to_hex};

/// Matches the ranged-pull size so one fetch produces one verifiable chunk.
pub const CHUNK_BYTES: NonZeroU64 = NonZeroU64::new(8_388_608).expect("8 MiB is non-zero");

/// SHA-256 digests in byte-offset order for a whole-blob-verified stage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkedDigest {
    /// Every chunk except the last has this byte length.
    pub chunk_size: u64,
    /// Lowercase hexadecimal SHA-256 digests in offset order.
    pub digests: Vec<String>,
}

impl ChunkedDigest {
    #[must_use]
    pub fn of(bytes: &[u8], chunk_size: NonZeroU64) -> Self {
        let span = usize::try_from(chunk_size.get()).unwrap_or(usize::MAX);
        let digests = bytes
            .chunks(span)
            .map(|chunk| Digest::of(chunk).as_str().to_owned())
            .collect();
        Self {
            chunk_size: chunk_size.get(),
            digests,
        }
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.digests.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.digests.is_empty()
    }

    /// Returns `None` when `index` lies past the last digest.
    #[must_use]
    pub fn range(&self, index: usize, total: u64) -> Option<Range<u64>> {
        if index >= self.digests.len() {
            return None;
        }
        let start = self.chunk_size.saturating_mul(index as u64);
        Some(start..start.saturating_add(self.chunk_size).min(total))
    }

    #[must_use]
    pub fn verify_chunk(&self, index: usize, bytes: &[u8]) -> bool {
        self.digests
            .get(index)
            .is_some_and(|expected| Digest::of(bytes).as_str() == expected)
    }
}

/// Builds chunk digests during whole-blob hashing to avoid reading staged bytes twice.
pub struct ChunkedDigestBuilder {
    chunk_size: u64,
    filled: u64,
    current: Sha256,
    digests: Vec<String>,
}

impl ChunkedDigestBuilder {
    #[must_use]
    pub fn new(chunk_size: NonZeroU64) -> Self {
        Self {
            chunk_size: chunk_size.get(),
            filled: 0,
            current: Sha256::new(),
            digests: Vec::new(),
        }
    }

    pub fn update(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            let remaining = self.chunk_size - self.filled;
            let take = usize::try_from(remaining.min(bytes.len() as u64)).unwrap_or(usize::MAX);
            self.current.update(&bytes[..take]);
            self.filled += take as u64;
            bytes = &bytes[take..];
            if self.filled == self.chunk_size {
                self.close_chunk();
            }
        }
    }

    fn close_chunk(&mut self) {
        let hasher = std::mem::replace(&mut self.current, Sha256::new());
        self.digests.push(to_hex(&hasher.finalize()));
        self.filled = 0;
    }

    /// Includes a non-empty trailing partial chunk.
    #[must_use]
    pub fn finish(mut self) -> ChunkedDigest {
        if self.filled > 0 {
            self.close_chunk();
        }
        ChunkedDigest {
            chunk_size: self.chunk_size,
            digests: self.digests,
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/blob/chunked/tests.rs"]
mod tests;
