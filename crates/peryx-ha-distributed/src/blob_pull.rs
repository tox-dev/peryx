//! Multi-source ranged pulls fall through failed sources. Whole-blob pulls trust no range until reassembly
//! verifies the expected digest; chunked pulls verify each range against trusted chunk metadata.

use bytes::Bytes;
use peryx_storage::blob::{ChunkedDigest, Digest};

use crate::blob::{BlobRequest, BlobTransport, ByteRange};
use crate::blob_piece::{PieceError, blob_piece};
use crate::blob_reassembly::{ReassemblyError, reassemble_verified};
use crate::peer::TransportError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullError {
    /// `failures` records each attempted source in source order.
    Exhausted {
        range: ByteRange,
        failures: Vec<(usize, TransportError)>,
    },
    Piece(PieceError),
    Reassembly(ReassemblyError),
}

/// Retries after a digest mismatch without the source that supplied corrupt ranges.
///
/// The caller must commit the returned bytes.
///
/// # Errors
/// [`PullError`] when every source is exhausted for a range, a fetched range is the wrong length, or no
/// source's bytes reassemble to `expected`.
///
pub async fn pull_ranged<T: BlobTransport + ?Sized>(
    sources: &[&T],
    digest: &Digest,
    ranges: &[ByteRange],
    total_length: usize,
    expected: &Digest,
) -> Result<Bytes, PullError> {
    let first_error = match reassemble_pass(sources, digest, ranges, total_length, expected).await {
        Ok(bytes) => return Ok(bytes),
        Err(error) => error,
    };
    for skip in 1..sources.len() {
        if let Ok(bytes) = reassemble_pass(&sources[skip..], digest, ranges, total_length, expected).await {
            return Ok(bytes);
        }
    }
    Err(first_error)
}

async fn reassemble_pass<T: BlobTransport + ?Sized>(
    sources: &[&T],
    digest: &Digest,
    ranges: &[ByteRange],
    total_length: usize,
    expected: &Digest,
) -> Result<Bytes, PullError> {
    let mut pieces = Vec::with_capacity(ranges.len());
    for &range in ranges {
        let bytes = fetch_range(sources, digest, range).await?;
        pieces.push(blob_piece(range.offset as u64, range.length as u64, bytes).map_err(PullError::Piece)?);
    }
    reassemble_verified(&pieces, total_length, expected).map_err(PullError::Reassembly)
}

// Bounds each ranged response before whole-blob verification.
const RANGED_CHUNK_BYTES: usize = 8 * 1024 * 1024;

/// `total_length` bounds reassembly allocation and must come from trusted metadata, not a peer.
///
/// # Errors
/// [`PullError`] when every source is exhausted for a range, a fetched range is the wrong length, or the
/// reassembled blob does not verify against `digest`.
pub async fn pull_ranged_blob<T: BlobTransport + ?Sized>(
    sources: &[&T],
    digest: &Digest,
    total_length: usize,
) -> Result<Bytes, PullError> {
    let ranges = chunk_ranges(total_length, RANGED_CHUNK_BYTES);
    pull_ranged(sources, digest, &ranges, total_length, digest).await
}

/// A zero-length blob yields no ranges and verifies as empty.
#[must_use]
pub fn chunk_ranges(total_length: usize, chunk: usize) -> Vec<ByteRange> {
    let mut ranges = Vec::with_capacity(total_length.div_ceil(chunk.max(1)));
    let mut offset = 0;
    while offset < total_length {
        let length = chunk.min(total_length - offset);
        ranges.push(ByteRange { offset, length });
        offset += length;
    }
    ranges
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChunkFailure {
    Transport(TransportError),
    WrongLength { expected: usize, got: usize },
    DigestMismatch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkUnavailable {
    pub index: usize,
    pub range: ByteRange,
    /// Failures in source order.
    pub failures: Vec<(usize, ChunkFailure)>,
}

impl ChunkUnavailable {
    /// Returns retryable transport failures with indices into the original source slice.
    #[must_use]
    pub fn transport_failures(&self) -> Vec<(usize, TransportError)> {
        self.failures
            .iter()
            .filter_map(|(index, failure)| match failure {
                ChunkFailure::Transport(error) => Some((*index, error.clone())),
                ChunkFailure::WrongLength { .. } | ChunkFailure::DigestMismatch => None,
            })
            .collect()
    }
}

/// Verifies the chunk against trusted metadata before returning it for staging. Transport, length, and
/// digest failures fall through to the next source. This function commits nothing.
///
/// # Errors
/// [`ChunkUnavailable`] when every source fails the chunk, carrying each source's failure in source order.
///
/// # Panics
/// Panics when `index` is outside `0..chunked.len()`.
pub async fn pull_chunk_verified<T: BlobTransport + ?Sized>(
    sources: &[&T],
    digest: &Digest,
    chunked: &ChunkedDigest,
    index: usize,
    total_length: u64,
) -> Result<Bytes, ChunkUnavailable> {
    let span = chunked
        .range(index, total_length)
        .expect("index is within the chunk count");
    let range = ByteRange {
        offset: usize::try_from(span.start).unwrap_or(usize::MAX),
        length: usize::try_from(span.end - span.start).unwrap_or(usize::MAX),
    };
    let mut failures = Vec::new();
    for (source_index, source) in sources.iter().enumerate() {
        let request = BlobRequest {
            digest: digest.clone(),
            range: Some(range),
        };
        match source.fetch_blob(request).await {
            Err(error) => failures.push((source_index, ChunkFailure::Transport(error))),
            Ok(bytes) if bytes.len() != range.length => failures.push((
                source_index,
                ChunkFailure::WrongLength {
                    expected: range.length,
                    got: bytes.len(),
                },
            )),
            Ok(bytes) if !chunked.verify_chunk(index, &bytes) => {
                failures.push((source_index, ChunkFailure::DigestMismatch));
            }
            Ok(bytes) => return Ok(Bytes::from(bytes)),
        }
    }
    Err(ChunkUnavailable { index, range, failures })
}

async fn fetch_range<T: BlobTransport + ?Sized>(
    sources: &[&T],
    digest: &Digest,
    range: ByteRange,
) -> Result<Vec<u8>, PullError> {
    let mut failures = Vec::new();
    for (index, source) in sources.iter().enumerate() {
        match source
            .fetch_blob(BlobRequest {
                digest: digest.clone(),
                range: Some(range),
            })
            .await
        {
            Ok(bytes) => return Ok(bytes),
            Err(error) => failures.push((index, error)),
        }
    }
    Err(PullError::Exhausted { range, failures })
}
