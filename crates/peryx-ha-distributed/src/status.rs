#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationStatus {
    Pending,
    Published,
    Failed,
    Expired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteRecord {
    pub published: bool,
    pub failed: bool,
    /// Retention deadline; `None` disables expiry.
    pub expiry: Option<u64>,
}

impl WriteRecord {
    /// Terminal states take precedence over expiry, keeping status stable after retention expiry or
    /// authority transfer.
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
