//! A chunked content digest: the sha256 of each fixed-size span of a blob.
//!
//! The whole-blob digest verifies a blob only once every byte is reassembled and hashed, so a ranged
//! fetch of a large blob cannot be trusted - and so cannot be forwarded to a client - until the whole
//! arrives. A chunked digest records the sha256 of each fixed span, computed when the blob was staged and
//! whole-verified, so a later incremental fetch verifies each chunk against its own digest and forwards it
//! before the rest of the blob arrives.
//!
//! The per-chunk digests are trusted because they are recorded from a whole-blob-verified stage: the same
//! bytes that hashed to the blob's identity produced them. A consumer that has them may stream a blob
//! chunk-by-chunk; one that does not falls back to whole-blob staging.

use std::num::NonZeroU64;
use std::ops::Range;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::{Digest, to_hex};

/// The default span each content chunk covers, aligned with the ranged-pull chunk so one ranged fetch maps
/// to one verifiable chunk.
pub const CHUNK_BYTES: NonZeroU64 = NonZeroU64::new(8 * 1024 * 1024).expect("8 MiB is non-zero");

/// The per-chunk digests of a blob under a fixed chunk size, in offset order.
///
/// Recorded when a blob is staged and whole-verified; consulted by an incremental read-through to verify
/// each fetched chunk against its own digest before forwarding it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkedDigest {
    /// The byte span each chunk covers. Every chunk but the last is exactly this long.
    pub chunk_size: u64,
    /// The lowercase-hex sha256 of each chunk, in offset order.
    pub digests: Vec<String>,
}

impl ChunkedDigest {
    /// Compute the chunked digest of `bytes` whole, under `chunk_size`.
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

    /// How many chunks the blob divides into.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.digests.len()
    }

    /// Whether the blob divides into no chunks, the empty blob.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.digests.is_empty()
    }

    /// The byte range chunk `index` covers within a blob of `total` bytes, or `None` past the last chunk.
    #[must_use]
    pub fn range(&self, index: usize, total: u64) -> Option<Range<u64>> {
        if index >= self.digests.len() {
            return None;
        }
        let start = self.chunk_size.saturating_mul(index as u64);
        Some(start..start.saturating_add(self.chunk_size).min(total))
    }

    /// Whether `bytes` are the content of chunk `index`, hashing to its recorded digest.
    #[must_use]
    pub fn verify_chunk(&self, index: usize, bytes: &[u8]) -> bool {
        self.digests
            .get(index)
            .is_some_and(|expected| Digest::of(bytes).as_str() == expected)
    }
}

/// Accumulates a [`ChunkedDigest`] from bytes streamed in arbitrary pieces, closing each chunk digest as
/// its boundary passes.
///
/// Mirrors how the content store streams a blob into its whole-blob hasher, so a stage computes the
/// per-chunk digests in the same pass it computes the whole one, without a second read of the bytes.
pub struct ChunkedDigestBuilder {
    chunk_size: u64,
    filled: u64,
    current: Sha256,
    digests: Vec<String>,
}

impl ChunkedDigestBuilder {
    /// Begin accumulating under `chunk_size`.
    #[must_use]
    pub fn new(chunk_size: NonZeroU64) -> Self {
        Self {
            chunk_size: chunk_size.get(),
            filled: 0,
            current: Sha256::new(),
            digests: Vec::new(),
        }
    }

    /// Feed `bytes`, closing and recording each chunk digest as its boundary passes.
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

    /// Finalize, recording the trailing partial chunk when any bytes remain in it.
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
mod tests;
