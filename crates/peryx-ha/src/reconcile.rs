use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcileEntry {
    pub source: String,
    pub epoch: u64,
    pub serial: u64,
    pub durably_committed: bool,
    pub already_applied: bool,
    pub superseded: bool,
    pub traceparent: Option<String>,
    pub outcome: Option<String>,
    pub updated_at_unix: i64,
}

impl ReconcileEntry {
    #[must_use]
    pub const fn is_pending(&self) -> bool {
        self.outcome.is_none()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileEnqueue {
    Enqueued,
    AlreadyPresent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewReconcileEntry<'a> {
    pub source: &'a str,
    pub epoch: u64,
    pub serial: u64,
    pub durably_committed: bool,
    pub already_applied: bool,
    pub superseded: bool,
    pub traceparent: Option<&'a str>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcilePage {
    pub records: Vec<(String, ReconcileEntry)>,
    pub next_cursor: Option<String>,
}

impl NewReconcileEntry<'_> {
    #[must_use]
    pub fn key(&self) -> String {
        format!("{}:{}:{}", self.source, self.epoch, self.serial)
    }

    #[must_use]
    pub fn record(&self, now: i64) -> ReconcileEntry {
        ReconcileEntry {
            source: self.source.to_owned(),
            epoch: self.epoch,
            serial: self.serial,
            durably_committed: self.durably_committed,
            already_applied: self.already_applied,
            superseded: self.superseded,
            traceparent: self.traceparent.map(str::to_owned),
            outcome: None,
            updated_at_unix: now,
        }
    }
}
