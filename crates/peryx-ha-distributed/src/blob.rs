//! Whole-blob fetches verify the streamed bytes against the requested digest. Ranged fetches cannot use
//! the whole-blob digest and remain unverified; callers must verify the reassembled blob before trusting,
//! serving, or committing it.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{Stream, StreamExt as _};
use peryx_storage::blob::Digest;
use tokio::sync::Semaphore;

use crate::peer::{TransferLimits, TransportError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub offset: usize,
    pub length: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobRequest {
    pub digest: Digest,
    pub range: Option<ByteRange>,
}

/// Whole-blob results are digest-verified; ranged results require verification after reassembly.
#[async_trait]
pub trait BlobTransport: Sync {
    /// # Errors
    /// [`TransportError::FrameTooLarge`] if the stream passes the byte cap before it ends,
    /// [`TransportError::DigestMismatch`] if a whole-blob fetch does not hash to its digest,
    /// [`TransportError::BlobNotFound`] if the peer holds no such blob, and
    /// [`TransportError::AtCapacity`] if the concurrent-stream limit is reached.
    async fn fetch_blob(&self, request: BlobRequest) -> Result<Vec<u8>, TransportError>;
}

/// Rejects the chunk that would exceed `cap`, without trusting an advertised length or growing the buffer
/// past the cap.
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

/// Rejects fetches beyond the configured stream limit with [`TransportError::AtCapacity`].
pub struct CapacityLimited<T> {
    inner: T,
    permits: Arc<Semaphore>,
}

impl<T> CapacityLimited<T> {
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

/// An in-process [`BlobTransport`] that applies the same byte cap and whole-blob verification as a peer.
pub struct LoopbackBlobSource {
    blobs: HashMap<Digest, Bytes>,
    limits: TransferLimits,
}

impl LoopbackBlobSource {
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
