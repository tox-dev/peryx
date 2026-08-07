//! The client half of peer artifact transfer: fetching a blob's bytes from a peer, streamed under a
//! byte cap and, for a whole-blob fetch, verified against the requested digest. This is the blob analog
//! of the metadata [`PeerTransport`], and it reuses the same [`TransportError`] taxonomy.
//!
//! # Integrity contract
//! A **whole-blob** fetch is verified in transport: the streamed bytes are hashed and compared to the
//! requested digest, and a mismatch fails closed with [`TransportError::DigestMismatch`], so a lying
//! peer cannot substitute content.
//!
//! A **ranged** fetch returns the requested bytes UNVERIFIED. A partial range cannot be checked against
//! the whole-blob sha256 (peryx keeps no per-chunk checksum), so the transport cannot vouch for it. The
//! caller MUST reassemble every range and digest-verify the whole blob — for example by committing it
//! through the blob store's `commit(digest)`, which is the verification point — before it trusts,
//! serves, or commits the bytes. Trusting ranged bytes directly defeats the integrity guarantee.
//!
//! [`PeerTransport`]: crate::peer::PeerTransport

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{Stream, StreamExt as _};
use peryx_storage::blob::Digest;
use tokio::sync::Semaphore;

use crate::peer::{TransferLimits, TransportError};

/// A byte range within a blob: `length` bytes starting at `offset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub offset: usize,
    pub length: usize,
}

/// A request to fetch a blob by digest, or a [`ByteRange`] of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobRequest {
    pub digest: Digest,
    pub range: Option<ByteRange>,
}

/// A source of blob bytes on a peer.
///
/// See the [module integrity contract](self#integrity-contract): a whole-blob fetch is digest-verified;
/// a ranged fetch is not, and the caller must verify the reassembled whole.
#[async_trait]
pub trait BlobTransport: Sync {
    /// Fetch `request.digest`, or the requested range of it, streamed under the transport's byte cap.
    ///
    /// # Errors
    /// [`TransportError::FrameTooLarge`] if the stream passes the byte cap before it ends,
    /// [`TransportError::DigestMismatch`] if a whole-blob fetch does not hash to its digest,
    /// [`TransportError::BlobNotFound`] if the peer holds no such blob, and
    /// [`TransportError::AtCapacity`] if the concurrent-stream limit is reached.
    async fn fetch_blob(&self, request: BlobRequest) -> Result<Vec<u8>, TransportError>;
}

/// Read `stream` into a buffer, stopping the moment the running total would pass `cap`.
///
/// The bound holds without trusting any advertised length: a peer that streams an unbounded body is
/// rejected before the buffer grows past the cap, never after it is fully read.
pub async fn collect_capped<S>(mut stream: S, cap: u64) -> Result<Vec<u8>, TransportError>
where
    S: Stream<Item = Bytes> + Unpin,
{
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let total = body.len() as u64 + chunk.len() as u64;
        if total > cap {
            return Err(TransportError::FrameTooLarge {
                limit: cap,
                actual: total,
            });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Bound the concurrent streams a wrapped [`BlobTransport`] runs.
///
/// A fetch past the limit fails closed with [`TransportError::AtCapacity`], so a slow or hostile peer
/// cannot open more than the configured number of streams at once.
pub struct CapacityLimited<T> {
    inner: T,
    permits: Arc<Semaphore>,
}

impl<T> CapacityLimited<T> {
    /// Wrap `inner`, admitting at most `max_concurrent` in-flight fetches.
    #[must_use]
    pub fn new(inner: T, max_concurrent: NonZeroUsize) -> Self {
        Self {
            inner,
            permits: Arc::new(Semaphore::new(max_concurrent.get())),
        }
    }
}

#[async_trait]
impl<T: BlobTransport + Send> BlobTransport for CapacityLimited<T> {
    async fn fetch_blob(&self, request: BlobRequest) -> Result<Vec<u8>, TransportError> {
        let _permit = Arc::clone(&self.permits)
            .try_acquire_owned()
            .map_err(|_| TransportError::AtCapacity)?;
        self.inner.fetch_blob(request).await
    }
}

/// An in-process [`BlobTransport`] over a fixed set of blobs, for driving a puller without a socket.
///
/// It streams the requested bytes under a byte cap and verifies a whole-blob fetch, so a test can
/// exercise the oversize and digest-mismatch paths by seeding oversized or mislabeled content.
pub struct LoopbackBlobSource {
    blobs: HashMap<Digest, Bytes>,
    limits: TransferLimits,
}

impl LoopbackBlobSource {
    /// Build a source serving `blobs`, capping each fetch at `limits.max_encoded_bytes`.
    #[must_use]
    pub const fn new(blobs: HashMap<Digest, Bytes>, limits: TransferLimits) -> Self {
        Self { blobs, limits }
    }
}

#[async_trait]
impl BlobTransport for LoopbackBlobSource {
    async fn fetch_blob(&self, request: BlobRequest) -> Result<Vec<u8>, TransportError> {
        let content = self
            .blobs
            .get(&request.digest)
            .ok_or_else(|| TransportError::BlobNotFound {
                digest: request.digest.as_str().to_owned(),
            })?;
        let served = request.range.map_or_else(
            || content.clone(),
            |range| {
                let start = range.offset.min(content.len());
                let end = start.saturating_add(range.length).min(content.len());
                content.slice(start..end)
            },
        );
        let stream = futures_util::stream::iter([served]);
        let bytes = collect_capped(stream, self.limits.max_encoded_bytes.get()).await?;
        if request.range.is_none() {
            let actual = Digest::of(&bytes);
            if actual != request.digest {
                return Err(TransportError::DigestMismatch {
                    expected: request.digest.as_str().to_owned(),
                    actual: actual.as_str().to_owned(),
                });
            }
        }
        Ok(bytes)
    }
}
