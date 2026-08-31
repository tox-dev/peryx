use std::num::{NonZeroU64, NonZeroUsize};
use std::sync::Mutex;

use async_trait::async_trait;
pub use peryx_ha::TransportError;

use crate::change_page::MAX_CHANGE_PAGE_BYTES;
use crate::http::constant_time_eq;
use crate::protocol::{Change, ChangePage, PROTOCOL_VERSION};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferLimits {
    pub max_operations: NonZeroUsize,
    pub max_encoded_bytes: NonZeroU64,
}

/// The change feed's byte bound is the writer's own, so a page it builds is never one this client
/// rejects whole.
pub const DEFAULT_TRANSFER_LIMITS: TransferLimits = TransferLimits {
    max_operations: NonZeroUsize::new(256).expect("256 is non-zero"),
    max_encoded_bytes: NonZeroU64::new(MAX_CHANGE_PAGE_BYTES).expect("the change page bound is non-zero"),
};

impl Default for TransferLimits {
    fn default() -> Self {
        DEFAULT_TRANSFER_LIMITS
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchRequest {
    pub after: u64,
    pub max_operations: NonZeroUsize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchFrame {
    page: ChangePage,
    encoded_len: u64,
}

impl BatchFrame {
    /// # Panics
    /// Panics if `page` cannot be serialized to JSON.
    #[must_use]
    pub fn new(page: ChangePage) -> Self {
        let encoded_len = serde_json::to_vec(&page).expect("a change page serializes").len() as u64;
        Self { page, encoded_len }
    }

    pub(crate) const fn from_encoded(page: ChangePage, encoded_len: u64) -> Self {
        Self { page, encoded_len }
    }

    #[must_use]
    pub const fn page(&self) -> &ChangePage {
        &self.page
    }

    #[must_use]
    pub fn frontier(&self) -> (&str, u64) {
        (&self.page.source, self.page.current_serial)
    }

    #[must_use]
    pub const fn encoded_len(&self) -> u64 {
        self.encoded_len
    }
}

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

/// Advancing an acknowledged frontier requires durable apply and commit. Advancing it on receipt can
/// replay committed changes after a new drain.
#[async_trait]
pub trait PeerTransport: Sync {
    /// # Errors
    /// Returns a retryable [`TransportError`] on transport loss and a terminal one on a credential,
    /// bound, or framing violation.
    async fn fetch_batch(&self, request: BatchRequest) -> Result<BatchFrame, TransportError>;
}

pub struct LoopbackPeer {
    source: String,
    token: String,
    limits: TransferLimits,
    log: Vec<Change>,
    fault: Mutex<Option<PeerFault>>,
}

impl LoopbackPeer {
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

    pub fn append(&mut self, event: Vec<u8>) {
        let serial = self.log.len() as u64 + 1;
        self.log.push(Change {
            serial,
            event,
            metadata: Vec::new(),
            blobs: Vec::new(),
        });
    }

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

pub struct LoopbackTransport<'peer> {
    peer: &'peer LoopbackPeer,
    token: String,
}

impl<'peer> LoopbackTransport<'peer> {
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

/// `through` is the serial reached by `changes`, not an acknowledged frontier. Persist it after durable
/// apply and commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontierSync {
    pub source: String,
    pub through: u64,
    pub changes: Vec<Change>,
    pub caught_up: bool,
}

/// `from` must be the durable frontier. `request_size` bounds each batch; `budget` bounds retained
/// changes. Persist the returned [`FrontierSync::through`] after durable apply and commit.
///
/// # Errors
/// Returns [`TransportError::SourceChanged`] if peer identity changes,
/// [`TransportError::FrontierGap`] for a non-contiguous batch, or any transport error.
pub async fn drain_to_frontier<T: PeerTransport>(
    transport: &T,
    from: u64,
    request_size: NonZeroUsize,
    budget: NonZeroUsize,
) -> Result<FrontierSync, TransportError> {
    let mut after = from;
    let mut changes: Vec<Change> = Vec::new();
    let mut max_operations = request_size.min(budget);
    let mut frame = transport.fetch_batch(BatchRequest { after, max_operations }).await?;
    let source = frame.page().source.clone();
    loop {
        let page = frame.page();
        validate_batch_size(max_operations, page)?;
        if source != page.source {
            return Err(TransportError::SourceChanged {
                expected: source,
                actual: page.source.clone(),
            });
        }
        let (reached, caught_up) = validate_contiguous(after, page)?;
        changes.extend(page.changes.iter().cloned());
        after = reached;
        if caught_up {
            return Ok(FrontierSync {
                source,
                through: after,
                changes,
                caught_up: true,
            });
        }
        let Some(remaining) = budget.get().checked_sub(changes.len()).and_then(NonZeroUsize::new) else {
            return Ok(FrontierSync {
                source,
                through: after,
                changes,
                caught_up: false,
            });
        };
        max_operations = request_size.min(remaining);
        frame = transport.fetch_batch(BatchRequest { after, max_operations }).await?;
    }
}

pub const fn validate_batch_size(limit: NonZeroUsize, page: &ChangePage) -> Result<(), TransportError> {
    if page.changes.len() > limit.get() {
        return Err(TransportError::TooManyOperations {
            limit: limit.get(),
            actual: page.changes.len(),
        });
    }
    Ok(())
}

/// Requires `page.after == from`, contiguous change serials, and a nonempty page while the advertised
/// frontier is ahead. Returns the reached serial and whether it meets that frontier.
///
/// # Errors
/// Returns [`TransportError::FrontierGap`] for a cursor or serial gap, and
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
