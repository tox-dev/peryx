//! The client-facing status of an admitted write.
//!
//! A write admitted at one DC can outlive the HTTP request that opened it, so a client polls for the
//! result. This derives that result from the write's durable lifecycle. A write that finalized reads as
//! [`Published`](OperationStatus::Published) and one that gave up as [`Failed`](OperationStatus::Failed);
//! both are terminal and stay put, so a client polling across an authority transfer or past the
//! retention window sees one final result rather than a status that flips back. A write that never
//! finalized and outlived its retention deadline reads [`Expired`](OperationStatus::Expired), and one
//! still in flight reads [`Pending`](OperationStatus::Pending).
//!
//! The classification is pure. Reading the durable write record, scoping the operation id to the
//! caller's authority, and the polling and retry-header surface are the status resource's wiring,
//! deferred to later work.

/// The status a client sees for an admitted write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationStatus {
    /// Still in flight: neither finalized nor expired.
    Pending,
    /// Finalized at the home. Terminal.
    Published,
    /// Gave up before finalizing. Terminal.
    Failed,
    /// Never finalized and outlived its retention deadline.
    Expired,
}

/// The durable lifecycle of one admitted write, from which its [`status`](WriteRecord::status) derives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteRecord {
    /// The write finalized at the home DC.
    pub published: bool,
    /// The write gave up before finalizing.
    pub failed: bool,
    /// The instant the write's retention window closes, or `None` when it has no deadline.
    pub expiry: Option<u64>,
}

impl WriteRecord {
    /// The client-facing [`OperationStatus`] at `now`.
    ///
    /// A finalized write is [`Published`](OperationStatus::Published) and one that gave up is
    /// [`Failed`](OperationStatus::Failed), each terminal and independent of `now`, so re-reading after
    /// an authority transfer or past the retention window returns the same result. Only a write that
    /// reached neither terminal state can be [`Expired`](OperationStatus::Expired), once `now` reaches
    /// its retention deadline; otherwise it is [`Pending`](OperationStatus::Pending).
    #[must_use]
    pub fn status(&self, now: u64) -> OperationStatus {
        if self.published {
            OperationStatus::Published
        } else if self.failed {
            OperationStatus::Failed
        } else if self.expiry.is_some_and(|expiry| now >= expiry) {
            OperationStatus::Expired
        } else {
            OperationStatus::Pending
        }
    }
}
