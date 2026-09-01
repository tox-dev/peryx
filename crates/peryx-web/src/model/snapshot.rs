use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UiSnapshot {
    pub version: String,
    /// Absent when the caller may not see it and when the metadata store cannot report it; the page
    /// shows neither as a serial of zero.
    pub serial: Option<u64>,
    pub requests: u64,
    pub ecosystems: Vec<UiEcosystemSummary>,
    pub families: Vec<UiMetricFamily>,
    pub indexes: Vec<UiIndex>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UiEcosystemSummary {
    pub ecosystem: String,
    pub pages: u64,
    pub reads: u64,
    pub bytes: u64,
    pub rejected: u64,
    pub writes: u64,
    pub families: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UiMetricFamily {
    pub ecosystem: String,
    pub key: String,
    pub label: String,
    pub roles: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiIndex {
    pub name: String,
    pub route: String,
    /// The ecosystem identifier.
    pub ecosystem: String,
    /// The client-facing API endpoint this index is served at, produced by its ecosystem driver.
    pub endpoint: String,
    /// The role: `cached`, `hosted`, or `virtual`.
    pub kind: String,
    /// Member names for a virtual index; empty otherwise.
    pub layers: Vec<String>,
    /// Whether uploads are enabled (a hosted layer with a token).
    pub uploads: bool,
    /// For a virtual index: the layer uploads land in.
    pub upload_to: Option<String>,
    pub upstream: Option<UiUpstream>,
    pub hosted: Option<UiHosted>,
    pub summary_status: UiSummaryStatus,
    pub summary_error_class: Option<String>,
    pub resource_count: u64,
    pub write_count: u64,
    pub recent_writes: Vec<UiRecentWrite>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UiSummaryStatus {
    Available,
    Unavailable,
    #[default]
    Unsupported,
}

/// A cached index's upstream status, with credential material redacted by the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiUpstream {
    pub url: String,
    pub auth_kind: String,
    pub auth_redacted: Option<String>,
    pub status: String,
}

/// A hosted store's status, with upload-token values redacted by the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiHosted {
    pub volatile: bool,
    pub token_configured: bool,
    pub token_redacted: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiRecentWrite {
    pub resource: String,
    pub artifact: String,
    pub group: String,
    pub written_at: Option<String>,
    pub size: Option<u64>,
}
