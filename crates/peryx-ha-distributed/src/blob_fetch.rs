//! Fetches missing blobs without committing them. Whole-blob transport verifies each result; terminal
//! errors dominate retryable losses, while successful results remain available to commit.

use std::num::NonZeroUsize;

use futures_util::{StreamExt as _, stream};
use peryx_storage::blob::Digest;

use crate::blob::{BlobRequest, BlobTransport};
use crate::peer::TransportError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchOutcome {
    Complete,
    /// `reason` is a stable machine token; `digest` is the first terminal failure in request order.
    Failed {
        reason: &'static str,
        digest: Digest,
    },
    /// `pending` blobs hit a retryable transport loss or concurrency limit.
    Backpressured {
        pending: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchReport {
    /// Verified blobs in request order, including progress from failed or back-pressured passes.
    pub fetched: Vec<(Digest, Vec<u8>)>,
    pub outcome: FetchOutcome,
}

/// Runs at most `concurrency` whole-blob fetches. A terminal error wins over retryable losses and reports
/// the first failing digest in request order, independent of completion order.
pub async fn fetch_missing<T: BlobTransport>(
    transport: &T,
    missing: &[Digest],
    concurrency: NonZeroUsize,
) -> FetchReport {
    let mut results: Vec<(usize, Digest, Result<Vec<u8>, TransportError>)> =
        stream::iter(missing.iter().cloned().enumerate())
            .map(|(index, digest)| async move {
                let bytes = transport
                    .fetch_blob(BlobRequest {
                        digest: digest.clone(),
                        range: None,
                    })
                    .await;
                (index, digest, bytes)
            })
            .buffer_unordered(concurrency.get())
            .collect()
            .await;
    results.sort_by_key(|(index, _, _)| *index);

    let mut fetched = Vec::new();
    let mut terminal: Option<(&'static str, Digest)> = None;
    let mut pending = 0;
    for (_, digest, result) in results {
        match result {
            Ok(bytes) => fetched.push((digest, bytes)),
            Err(error) => match error.terminal_reason() {
                Some(reason) if terminal.is_none() => terminal = Some((reason, digest)),
                Some(_) => {}
                None => pending += 1,
            },
        }
    }
    let outcome = match (terminal, pending) {
        (Some((reason, digest)), _) => FetchOutcome::Failed { reason, digest },
        (None, 0) => FetchOutcome::Complete,
        (None, pending) => FetchOutcome::Backpressured { pending },
    };
    FetchReport { fetched, outcome }
}
