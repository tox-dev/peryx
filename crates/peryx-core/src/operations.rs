use serde::{Deserialize, Serialize};

use crate::view::UiOperationStatus;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OperationsHealth {
    pub pending: u64,
    pub published: u64,
    pub failed: u64,
    pub expired: u64,
    pub total: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationRow {
    pub operation: String,
    pub status: UiOperationStatus,
    pub updated_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationsView {
    pub captured_at: i64,
    pub health: OperationsHealth,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows: Option<Vec<OperationRow>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[cfg(test)]
#[path = "../tests/unit/operations/tests.rs"]
mod tests;
