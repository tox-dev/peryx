//! A concurrent pull for the blobs a replica is missing, the blob analog of [`advance_once`].
//!
//! [`fetch_missing`] drives one [`BlobTransport`] over a set of missing digests, running up to a
//! concurrency bound of whole-blob fetches at once. Each fetch is a whole-blob request, so the
//! transport digest-verifies it (see the [blob integrity contract](crate::blob)) and a returned buffer
//! already hashes to the digest that named it. The step keeps no clock and commits nothing: it hands the
//! verified bytes back for the caller to commit and classifies the pass so the caller knows whether to
//! commit, fail closed, or retry the remainder.
//!
//! Compose the transport behind [`CapacityLimited`](crate::blob::CapacityLimited) to cap in-flight
//! streams: a fetch past that cap returns the retryable [`TransportError::AtCapacity`], which this pass
//! reports as back-pressure rather than a terminal loss.
//!
//! [`advance_once`]: crate::driver::advance_once

use std::num::NonZeroUsize;

use futures_util::{StreamExt as _, stream};
use peryx_storage::blob::Digest;

use crate::blob::{BlobRequest, BlobTransport};
use crate::peer::TransportError;

/// What one [`fetch_missing`] pass made of the missing set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchOutcome {
    /// Every requested digest fetched and verified.
    Complete,
    /// A blob failed terminally, so the pull fails closed. `reason` is the stable machine token and
    /// `digest` the first such blob in request order, chosen independent of completion order.
    Failed { reason: &'static str, digest: Digest },
    /// No terminal failure, but `pending` blobs hit a retryable loss (a transport drop or the transport's
    /// concurrency cap). The caller retries just those.
    Backpressured { pending: usize },
}

/// The verified blobs a pass fetched, paired with its [`FetchOutcome`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchReport {
    /// Each fetched blob's digest and verified bytes, in request order, ready to commit.
    pub fetched: Vec<(Digest, Vec<u8>)>,
    pub outcome: FetchOutcome,
}

/// Fetch every digest in `missing` from `transport`, up to `concurrency` whole-blob fetches at once.
///
/// A terminal failure dominates: the outcome is [`FetchOutcome::Failed`] with the first failing blob in
/// request order, whatever else the concurrent pass produced, so a lying peer cannot hide behind a
/// racing success. Absent any terminal failure, a retryable loss yields [`FetchOutcome::Backpressured`]
/// and a clean pass [`FetchOutcome::Complete`]. The returned bytes are the blobs that fetched, so a
/// caller may still commit the progress a failed or back-pressured pass made.
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
