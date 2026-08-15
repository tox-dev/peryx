use serde::{Deserialize, Serialize};

use crate::view::{UiArtifactSource, UiByteAvailability};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PlacementHealth {
    pub local: u64,
    pub remote_only: u64,
    pub unavailable: u64,
    pub total: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementRow {
    pub digest: String,
    pub source: UiArtifactSource,
    pub availability: UiByteAvailability,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementView {
    pub captured_at: i64,
    pub health: PlacementHealth,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows: Option<Vec<PlacementRow>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// Only `Verified` copies can serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlobPlacementStatus {
    Pending,
    Verified,
    Failed,
    Revoked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobDatacenterPlacement {
    pub data_center: String,
    pub status: BlobPlacementStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobPlacementView {
    pub digest: String,
    pub datacenters: Vec<BlobDatacenterPlacement>,
}

#[cfg(test)]
#[path = "../tests/unit/placement/tests.rs"]
mod tests;
