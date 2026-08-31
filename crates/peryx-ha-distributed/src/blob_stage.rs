//! Schedules a ranged blob pull over every verified source at once and writes each completed range
//! straight into an unpublished stage. A pull never holds more than [`RangedPullBudget`] bytes, whatever
//! the blob's size, and publishes nothing before [`BlobWrite::commit`](peryx_storage::blob::BlobWrite)
//! verifies the whole digest.
//!
//! Ranges are dispatched in offset order and each starts at a different source, so healthy sources share
//! the transfer instead of queueing behind the first one.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;

use bytes::Bytes;
use futures_util::StreamExt as _;
use futures_util::stream::FuturesUnordered;
use peryx_storage::blob::{
    BlobError, BlobErrorKind, BlobStorage, CHUNK_BYTES, ChunkedDigest, ChunkedDigestBuilder, Digest, PlacementReceipt,
};

use crate::blob::{BlobRequest, BlobTransport, ByteRange};
use crate::blob_pull::{ChunkFailure, ChunkUnavailable, chunk_ranges};

/// Matches the copy path's range size, so one request never outgrows what a peer serves in one response.
const RANGE_BYTES: usize = 8 * 1024 * 1024;

/// Bounds one pull's outstanding requests and the bytes it holds while ranges wait for their turn to be
/// written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RangedPullBudget {
    /// Bytes asked for in one request. A trusted catalog overrides it, because its chunk boundaries are
    /// the only ones a chunk digest verifies.
    pub range_bytes: NonZeroUsize,
    pub max_in_flight: NonZeroUsize,
    /// Covers in-flight requests and completed ranges held in the reorder window together.
    pub max_resident_bytes: NonZeroUsize,
}

/// Four 8 MiB ranges overlap at most. The blob plane pulls one blob at a time, so this also bounds a
/// whole pass.
pub const DEFAULT_RANGED_PULL_BUDGET: RangedPullBudget = RangedPullBudget {
    range_bytes: NonZeroUsize::new(RANGE_BYTES).expect("8 MiB is non-zero"),
    max_in_flight: NonZeroUsize::new(4).expect("4 is non-zero"),
    max_resident_bytes: NonZeroUsize::new(4 * RANGE_BYTES).expect("32 MiB is non-zero"),
};

/// A published pull.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedPull {
    pub receipt: PlacementReceipt,
    /// Chunk digests derived from the staged bytes, present exactly when the pull started without a
    /// trusted catalog. They come from the same pass that published the blob, so they describe the bytes
    /// the whole digest verified rather than whatever the store holds when a later reader asks.
    pub chunks: Option<ChunkedDigest>,
}

#[derive(Debug, thiserror::Error)]
pub enum StagedPullError {
    #[error("no source served the {} bytes at offset {}", .0.range.length, .0.range.offset)]
    RangeUnavailable(ChunkUnavailable),
    /// Right-length corruption verified by no trusted chunk digest attributes no source.
    #[error("no source assignment of {attempts} produced bytes matching the blob digest")]
    DigestMismatch { attempts: usize },
    #[error("the local stage could not accept the pulled blob: {0}")]
    Stage(#[source] BlobError),
}

/// Pulls `digest` from `sources` into `blobs` and publishes it only once the whole digest verifies.
///
/// `total_length` bounds the transfer and must come from a verified placement record rather than a peer
/// advertisement. `catalog` supplies trusted per-chunk digests when the local store holds them; each
/// range is then verified on arrival and a corrupt source is named. Without it a range is trusted only
/// as far as its length, a whole-digest failure retries under a rotated assignment because the mismatch
/// names no source, and the pass derives the chunk digests the next ranged read will verify against.
///
/// # Errors
/// [`StagedPullError::RangeUnavailable`] when no source serves a range, [`StagedPullError::DigestMismatch`]
/// when no assignment reassembles the blob, and [`StagedPullError::Stage`] when the local store cannot
/// stage or publish the bytes.
pub async fn pull_blob_staged<T: BlobTransport + ?Sized>(
    blobs: &BlobStorage,
    sources: &[&T],
    digest: &Digest,
    total_length: usize,
    catalog: Option<&ChunkedDigest>,
    budget: RangedPullBudget,
) -> Result<StagedPull, StagedPullError> {
    let ranges = plan_ranges(total_length, catalog, budget.range_bytes);
    // Every source already matched a trusted chunk digest, so a rotation would restage the same bytes.
    let attempts = if catalog.is_some() { 1 } else { sources.len().max(1) };
    for rotation in 0..attempts {
        if let Some(staged) = stage_pass(blobs, sources, digest, &ranges, catalog, budget, rotation).await? {
            return Ok(staged);
        }
    }
    Err(StagedPullError::DigestMismatch { attempts })
}

/// Returns `None` when the staged bytes failed whole-digest verification, leaving nothing published.
async fn stage_pass<T: BlobTransport + ?Sized>(
    blobs: &BlobStorage,
    sources: &[&T],
    digest: &Digest,
    ranges: &[ByteRange],
    catalog: Option<&ChunkedDigest>,
    budget: RangedPullBudget,
    rotation: usize,
) -> Result<Option<StagedPull>, StagedPullError> {
    let mut stage = blobs.begin().await.map_err(StagedPullError::Stage)?;
    let mut derived = catalog.is_none().then(|| ChunkedDigestBuilder::new(CHUNK_BYTES));
    let mut dispatched = 0;
    let mut next_write = 0;
    let mut resident = 0;
    let mut window: BTreeMap<usize, Bytes> = BTreeMap::new();
    let mut in_flight = FuturesUnordered::new();
    loop {
        while dispatched < ranges.len()
            && in_flight.len() < budget.max_in_flight.get()
            && admits(resident, ranges[dispatched].length, budget)
        {
            resident += ranges[dispatched].length;
            in_flight.push(fetch_range(
                sources,
                digest,
                catalog,
                dispatched,
                ranges[dispatched],
                rotation,
            ));
            dispatched += 1;
        }
        let Some((index, bytes)) = in_flight
            .next()
            .await
            .transpose()
            .map_err(StagedPullError::RangeUnavailable)?
        else {
            break;
        };
        window.insert(index, bytes);
        while let Some(bytes) = window.remove(&next_write) {
            resident -= bytes.len();
            if let Some(builder) = &mut derived {
                builder.update(&bytes);
            }
            stage.write_chunk(bytes).await.map_err(StagedPullError::Stage)?;
            next_write += 1;
        }
    }
    match stage.commit(digest).await {
        Ok(receipt) => Ok(Some(StagedPull {
            receipt,
            chunks: derived.map(ChunkedDigestBuilder::finish),
        })),
        Err(error) if error.kind() == BlobErrorKind::DigestMismatch => Ok(None),
        Err(error) => Err(StagedPullError::Stage(error)),
    }
}

/// An idle pipeline admits its next range whatever the budget, so a range wider than the whole budget
/// still makes progress instead of deadlocking.
const fn admits(resident: usize, length: usize, budget: RangedPullBudget) -> bool {
    resident == 0 || resident + length <= budget.max_resident_bytes.get()
}

/// A trusted catalog owns the range boundaries so each response verifies against one chunk digest.
fn plan_ranges(total_length: usize, catalog: Option<&ChunkedDigest>, range_bytes: NonZeroUsize) -> Vec<ByteRange> {
    let Some(catalog) = catalog else {
        return chunk_ranges(total_length, range_bytes.get());
    };
    (0..catalog.len())
        .map(|index| {
            let span = catalog
                .range(index, total_length as u64)
                .expect("index is within the chunk count");
            ByteRange {
                offset: usize::try_from(span.start).unwrap_or(usize::MAX),
                length: usize::try_from(span.end - span.start).unwrap_or(usize::MAX),
            }
        })
        .collect()
}

/// Starts each range at a different source and walks the rest in order after a failure, so one slow or
/// broken source neither serves the whole blob nor blocks the ranges assigned elsewhere.
async fn fetch_range<T: BlobTransport + ?Sized>(
    sources: &[&T],
    digest: &Digest,
    catalog: Option<&ChunkedDigest>,
    index: usize,
    range: ByteRange,
    rotation: usize,
) -> Result<(usize, Bytes), ChunkUnavailable> {
    let mut failures = Vec::new();
    for attempt in 0..sources.len() {
        let source = (index + rotation + attempt) % sources.len();
        let request = BlobRequest {
            digest: digest.clone(),
            range: Some(range),
        };
        match sources[source].fetch_blob(request).await {
            Err(error) => failures.push((source, ChunkFailure::Transport(error))),
            Ok(bytes) if bytes.len() != range.length => failures.push((
                source,
                ChunkFailure::WrongLength {
                    expected: range.length,
                    got: bytes.len(),
                },
            )),
            Ok(bytes) if catalog.is_some_and(|catalog| !catalog.verify_chunk(index, &bytes)) => {
                failures.push((source, ChunkFailure::DigestMismatch));
            }
            Ok(bytes) => return Ok((index, Bytes::from(bytes))),
        }
    }
    Err(ChunkUnavailable { index, range, failures })
}
