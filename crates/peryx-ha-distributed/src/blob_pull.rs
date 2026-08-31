//! Range planning and the failure record a multi-source ranged pull reports. Bytes never accumulate
//! here: [`pull_blob_staged`](crate::blob_stage::pull_blob_staged) streams each range into a stage.

use crate::blob::ByteRange;
use crate::peer::TransportError;

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
    /// Each attempted source in attempt order, indexed into the caller's source slice.
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
