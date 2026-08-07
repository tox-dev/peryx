use std::num::{NonZeroU64, NonZeroUsize};
use std::sync::Mutex;

use async_trait::async_trait;
pub use peryx_ha::TransportError;

use crate::http::constant_time_eq;
use crate::protocol::{Change, ChangePage, PROTOCOL_VERSION};

/// The bounds a peer enforces before it frames a batch, so one slow or oversized peer cannot force
/// unbounded work on the transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferLimits {
    pub max_operations: NonZeroUsize,
    pub max_encoded_bytes: NonZeroU64,
}

/// A conservative default: at most 256 operations and 4 MiB per framed batch.
pub const DEFAULT_TRANSFER_LIMITS: TransferLimits = TransferLimits {
    max_operations: NonZeroUsize::new(256).expect("256 is non-zero"),
    max_encoded_bytes: NonZeroU64::new(4 * 1024 * 1024).expect("4 MiB is non-zero"),
};

impl Default for TransferLimits {
    fn default() -> Self {
        DEFAULT_TRANSFER_LIMITS
    }
}

/// A client request for the next bounded batch of changes after a frontier serial.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchRequest {
    pub after: u64,
    pub max_operations: NonZeroUsize,
}

/// A bounded, framed batch a peer delivered, carrying its encoded size so the caller can account for
/// transfer without re-serializing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchFrame {
    page: ChangePage,
    encoded_len: u64,
}

impl BatchFrame {
    /// Frame a page, recording the encoded byte length that limits count against.
    ///
    /// # Panics
    /// Panics only if the page fails to serialize to JSON, which a well-formed [`ChangePage`] never
    /// does.
    #[must_use]
    pub fn new(page: ChangePage) -> Self {
        let encoded_len = serde_json::to_vec(&page).expect("a change page serializes").len() as u64;
        Self { page, encoded_len }
    }

    /// The decoded change page.
    #[must_use]
    pub const fn page(&self) -> &ChangePage {
        &self.page
    }

    /// The source identity and the serial the peer advertises as its current frontier.
    #[must_use]
    pub fn frontier(&self) -> (&str, u64) {
        (&self.page.source, self.page.current_serial)
    }

    /// The framed byte length that counted against the encoded-byte limit.
    #[must_use]
    pub const fn encoded_len(&self) -> u64 {
        self.encoded_len
    }
}

/// A deterministic transport fault a test injects to exercise the retryable arms without a live
/// network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerFault {
    Disconnect,
    Timeout,
}

impl PeerFault {
    const fn into_error(self) -> TransportError {
        match self {
            Self::Disconnect => TransportError::Disconnected,
            Self::Timeout => TransportError::Timeout,
        }
    }
}

/// The authenticated boundary a replica pulls bounded change batches across. Transport, credentials,
/// and connection lifecycle live behind it; the caller only advances a frontier.
///
/// Frontier durability contract: a puller advances its acknowledged frontier ONLY after a batch is
/// durably applied and committed, never on mere receipt. A producer relies on this to never resend
/// below an acknowledged serial, so downstream dedup and interval compaction stay correct. Advancing
/// on transfer instead of on durable apply lets a re-drain replay committed changes.
#[async_trait]
pub trait PeerTransport: Sync {
    /// Fetch the next bounded batch after `request.after`.
    ///
    /// # Errors
    /// Returns a retryable [`TransportError`] on transport loss and a terminal one on a credential,
    /// bound, or framing violation.
    async fn fetch_batch(&self, request: BatchRequest) -> Result<BatchFrame, TransportError>;
}

/// An in-process peer holding an ordered change log behind a bearer credential.
///
/// It frames batches the same way the HTTP primary does, enforcing operation and encoded-byte
/// bounds, so a replica can be driven deterministically without a socket.
pub struct LoopbackPeer {
    source: String,
    token: String,
    limits: TransferLimits,
    log: Vec<Change>,
    fault: Mutex<Option<PeerFault>>,
}

impl LoopbackPeer {
    /// Build an empty peer that answers for `source` behind `token`.
    #[must_use]
    pub fn new(source: impl Into<String>, token: impl Into<String>, limits: TransferLimits) -> Self {
        Self {
            source: source.into(),
            token: token.into(),
            limits,
            log: Vec::new(),
            fault: Mutex::new(None),
        }
    }

    /// Append the next contiguous change, assigning it the following serial.
    pub fn append(&mut self, event: Vec<u8>) {
        let serial = self.log.len() as u64 + 1;
        self.log.push(Change {
            serial,
            event,
            metadata: Vec::new(),
            blobs: Vec::new(),
        });
    }

    /// Arm the peer to fail its next answered request, then clear the fault. Deterministic stand-in
    /// for transient transport loss.
    #[cfg(test)]
    pub fn inject(&self, fault: PeerFault) {
        *self.fault.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(fault);
    }

    fn take_fault(&self) -> Option<PeerFault> {
        self.fault
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

/// A client bound to one [`LoopbackPeer`] and the credential it presents.
pub struct LoopbackTransport<'peer> {
    peer: &'peer LoopbackPeer,
    token: String,
}

impl<'peer> LoopbackTransport<'peer> {
    /// Bind a client presenting `token` to `peer`.
    #[must_use]
    pub fn connect(peer: &'peer LoopbackPeer, token: impl Into<String>) -> Self {
        Self {
            peer,
            token: token.into(),
        }
    }
}

#[async_trait]
impl PeerTransport for LoopbackTransport<'_> {
    async fn fetch_batch(&self, request: BatchRequest) -> Result<BatchFrame, TransportError> {
        if let Some(fault) = self.peer.take_fault() {
            return Err(fault.into_error());
        }
        if !constant_time_eq(self.token.as_bytes(), self.peer.token.as_bytes()) {
            return Err(TransportError::Unauthenticated);
        }
        let limit = self.peer.limits.max_operations;
        if request.max_operations > limit {
            return Err(TransportError::TooManyOperations {
                limit: limit.get(),
                actual: request.max_operations.get(),
            });
        }
        let changes: Vec<Change> = self
            .peer
            .log
            .iter()
            .filter(|change| change.serial > request.after)
            .take(request.max_operations.get())
            .cloned()
            .collect();
        let page = ChangePage {
            version: PROTOCOL_VERSION,
            source: self.peer.source.clone(),
            after: request.after,
            current_serial: self.peer.log.len() as u64,
            changes,
        };
        let frame = BatchFrame::new(page);
        let cap = self.peer.limits.max_encoded_bytes.get();
        if frame.encoded_len() > cap {
            return Err(TransportError::FrameTooLarge {
                limit: cap,
                actual: frame.encoded_len(),
            });
        }
        Ok(frame)
    }
}

/// The bounded result of draining a peer toward its advertised frontier.
///
/// `through` is the serial the returned `changes` reach, not an acknowledged frontier.
///
/// The caller
/// durably applies and commits the changes, persists `through` as the new frontier only then, and
/// resumes the next drain from the persisted value. Persisting `through` before a durable apply
/// breaks the trait's frontier durability contract and lets a re-drain replay committed changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontierSync {
    pub source: String,
    pub through: u64,
    pub changes: Vec<Change>,
    pub caught_up: bool,
}

/// Pull contiguous batches from `transport`, advancing from serial `from` toward the peer's frontier.
///
/// `request_size` bounds one batch and `budget` bounds retained changes: on reaching the budget the
/// drain returns early with `caught_up = false`, so a caller commits and resumes rather than buffer
/// an unbounded log from a fast peer.
///
/// `from` must be the caller's durably persisted frontier, and the returned [`FrontierSync::through`]
/// becomes the next frontier only after the batch is durably applied and committed, per the
/// [`PeerTransport`] durability contract.
///
/// # Errors
/// Returns [`TransportError::SourceChanged`] if the peer identity moves mid-drain,
/// [`TransportError::FrontierGap`] if a batch is non-contiguous, or any error the transport raises.
pub async fn drain_to_frontier<T: PeerTransport>(
    transport: &T,
    from: u64,
    request_size: NonZeroUsize,
    budget: NonZeroUsize,
) -> Result<FrontierSync, TransportError> {
    let mut after = from;
    let mut source: Option<String> = None;
    let mut changes: Vec<Change> = Vec::new();
    loop {
        let frame = transport
            .fetch_batch(BatchRequest {
                after,
                max_operations: request_size,
            })
            .await?;
        let page = frame.page();
        if let Some(known) = &source {
            if known != &page.source {
                return Err(TransportError::SourceChanged {
                    expected: known.clone(),
                    actual: page.source.clone(),
                });
            }
        } else {
            source = Some(page.source.clone());
        }
        let (reached, caught_up) = validate_contiguous(after, page)?;
        changes.extend(page.changes.iter().cloned());
        after = reached;
        if caught_up {
            return Ok(FrontierSync {
                source: source.unwrap_or_default(),
                through: after,
                changes,
                caught_up: true,
            });
        }
        if changes.len() >= budget.get() {
            return Ok(FrontierSync {
                source: source.unwrap_or_default(),
                through: after,
                changes,
                caught_up: false,
            });
        }
    }
}

/// Check that `page` continues the frontier from `from`: its `after` matches the cursor, its change
/// serials run `from + 1, from + 2, ...` with no gap, and an empty page is not hiding a frontier still
/// ahead. Returns the serial the changes reach and whether they meet the peer's advertised frontier.
///
/// This is the one contiguity check both [`drain_to_frontier`] and the single-step driver run, so a
/// caller cannot skip serials or accept a stalled batch by taking a different validation path.
///
/// # Errors
/// Returns [`TransportError::FrontierGap`] for a mismatched cursor or a non-contiguous serial, and
/// [`TransportError::EmptyBatch`] for an empty page behind the advertised frontier.
pub fn validate_contiguous(from: u64, page: &ChangePage) -> Result<(u64, bool), TransportError> {
    if page.after != from {
        return Err(TransportError::FrontierGap {
            expected: from,
            actual: page.after,
        });
    }
    let frontier = page.current_serial;
    if page.changes.is_empty() && from < frontier {
        return Err(TransportError::EmptyBatch { frontier, after: from });
    }
    let mut reached = from;
    for change in &page.changes {
        if change.serial != reached + 1 {
            return Err(TransportError::FrontierGap {
                expected: reached + 1,
                actual: change.serial,
            });
        }
        reached = change.serial;
    }
    Ok((reached, reached >= frontier))
}
