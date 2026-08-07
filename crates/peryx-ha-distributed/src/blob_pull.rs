//! Drive a multi-source ranged fetch of one blob, the MODE-B counterpart to
//! [`fetch_missing`](crate::blob_fetch::fetch_missing).
//!
//! Where [`fetch_missing`](crate::blob_fetch::fetch_missing) drives one transport over the whole missing
//! set as whole-blob fetches, this pulls ONE blob as byte ranges across an ordered list of sources. For
//! each range it tries the sources in order, falling to the next on any loss so one stale or wrong
//! `Verified` peer cannot block a fetch another peer can serve, adapts each fetched range into a
//! [`BlobPiece`](crate::blob_reassembly::BlobPiece) through [`blob_piece`](crate::blob_piece::blob_piece),
//! and reassembles the whole under [`reassemble_verified`](crate::blob_reassembly::reassemble_verified).
//! The whole-blob digest check is the safety net that makes falling through safe. It commits nothing: the
//! caller commits the returned bytes, as it does for [`fetch_missing`](crate::blob_fetch::fetch_missing).

use bytes::Bytes;
use peryx_storage::blob::{ChunkedDigest, Digest};

use crate::blob::{BlobRequest, BlobTransport, ByteRange};
use crate::blob_piece::{PieceError, blob_piece};
use crate::blob_reassembly::{ReassemblyError, reassemble_verified};
use crate::peer::TransportError;

/// Why a multi-source ranged blob pull could not produce the verified whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullError {
    /// Every source failed a range. `failures` carries each source's index and its error in source
    /// order, so a caller can see which `Verified` peers failed and how without the pull having blocked
    /// on any one of them.
    Exhausted {
        range: ByteRange,
        failures: Vec<(usize, TransportError)>,
    },
    /// A fetched body was not the length its range declared.
    Piece(PieceError),
    /// The reassembled whole blob failed a structural check or digest verification.
    Reassembly(ReassemblyError),
}

/// Pull `digest` as `ranges` across `sources` in order and return the verified whole blob.
///
/// Each range is fetched from the first source that answers, falling to the next on any
/// [`TransportError`], so a down or stale peer never blocks a range another source serves. A peer that
/// answers with right-length wrong content is caught only when the reassembled whole fails its digest, so
/// on that failure the pull drops the source that led the pass and reassembles again from the rest until a
/// healthy source produces the verified blob or none remains. Running out of sources for a range is a
/// [`PullError::Exhausted`] carrying each source's failure. Each fetched range becomes a
/// [`BlobPiece`](crate::blob_reassembly::BlobPiece) through [`blob_piece`], which fails closed when a
/// source returns the wrong number of bytes, and [`reassemble_verified`] tiles the pieces and
/// digest-verifies the whole against `expected` — the check that keeps falling through safe. Nothing is
/// committed; the caller commits the returned bytes.
///
/// # Errors
/// [`PullError`] when every source is exhausted for a range, a fetched range is the wrong length, or no
/// source's bytes reassemble to `expected`.
///
/// # Panics
/// Never in practice: the loop runs at least one pass and records that pass's error, so the final
/// `expect` on an all-passes-failed result is unreachable.
pub async fn pull_ranged<T: BlobTransport + ?Sized>(
    sources: &[&T],
    digest: &Digest,
    ranges: &[ByteRange],
    total_length: usize,
    expected: &Digest,
) -> Result<Bytes, PullError> {
    // A ranged fetch carries no per-chunk checksum, so a `Verified` peer returning right-length wrong
    // content is caught only when the reassembled whole fails its digest. On that failure, drop the
    // source that led the pass and reassemble again from the rest: without this, `fetch_range` keeps
    // taking the poisoned peer's bytes for every range and the pull can never succeed even when a
    // healthy source holds the blob, which is exactly the resilience this module promises. Each pass
    // still falls through transport losses, so a down source never blocks a range another source serves.
    // Keep the first pass's error: it ran over every source, so its failure list is the fullest account
    // of why the blob could not be drawn, while a later pass sees only a subset.
    let mut error = None;
    for skip in 0..sources.len().max(1) {
        match reassemble_pass(&sources[skip..], digest, ranges, total_length, expected).await {
            Ok(bytes) => return Ok(bytes),
            Err(pass_error) => error = error.or(Some(pass_error)),
        }
    }
    Err(error.expect("the loop runs at least once and records the pass error"))
}

/// One reassembly pass over `sources`: draw every range, adapt it to a piece, and digest-verify the
/// tiled whole. The caller retries a failed pass over a reduced source set.
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

/// The byte span one ranged request covers, so a large blob is drawn in bounded pieces rather than a
/// single request that must buffer the whole body before it can be checked.
const RANGED_CHUNK_BYTES: usize = 8 * 1024 * 1024;

/// Pull the whole blob `digest` of `total_length` bytes across `sources` as fixed-size ranges and return
/// the verified whole.
///
/// This is the per-blob driver: it tiles `[0, total_length)` into [`RANGED_CHUNK_BYTES`] ranges and drives
/// [`pull_ranged`] over `sources`, so each range falls through to the next source on a loss and the whole
/// is digest-verified against `digest` before any byte is returned. `total_length` bounds the reassembly
/// pre-allocation, so the caller MUST pass the size from its own trusted metadata rather than a peer
/// advertisement. Nothing is committed; the caller commits the returned bytes.
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

/// Tile `[0, total_length)` into consecutive `chunk`-byte ranges, the last one short. A zero-length blob
/// yields no ranges, which [`reassemble_verified`] accepts as the empty blob.
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

/// How one source failed to serve a single chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChunkFailure {
    /// The source could not be reached or refused the range.
    Transport(TransportError),
    /// The source returned a body of the wrong length for the range.
    WrongLength { expected: usize, got: usize },
    /// The source returned right-length bytes that did not hash to the chunk's recorded digest.
    DigestMismatch,
}

/// Why one chunk of a blob could not be drawn from any source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkUnavailable {
    /// The chunk index within the blob's [`ChunkedDigest`].
    pub index: usize,
    /// The byte span the chunk covers.
    pub range: ByteRange,
    /// Each source's failure, in source order.
    pub failures: Vec<(usize, ChunkFailure)>,
}

impl ChunkUnavailable {
    /// The transport-level failures among the sources, so a caller can decide whether a retry could
    /// recover: a source that failed only its chunk digest will not, one that failed a reachable transport
    /// might. The index is into the same source slice the pull ran over.
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

/// Draw chunk `index` of `chunked` for `digest` across `sources` in order, returning its verified bytes.
///
/// The per-chunk counterpart to [`pull_ranged`]: where that reassembles the whole blob and digest-verifies
/// it once at the end, this verifies each chunk against its own recorded digest as it arrives, so a chunk
/// is trusted — and can be staged or forwarded — before the rest of the blob is drawn. A source that
/// disconnects, returns the wrong number of bytes, or returns right-length bytes that fail the chunk digest
/// is skipped for the next, so one bad source never blocks a chunk another can serve. The recorded
/// per-chunk digests are trusted metadata a node wrote from whole-verified bytes, so a source cannot forge
/// a chunk past them. Nothing is committed; the caller stages the returned bytes.
///
/// # Errors
/// [`ChunkUnavailable`] when every source fails the chunk, carrying each source's failure in source order.
///
/// # Panics
/// Never in practice: the caller draws `index` in `0..chunked.len()`, for which
/// [`ChunkedDigest::range`] returns the covered span.
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
