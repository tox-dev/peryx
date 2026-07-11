use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{string_at, u64_at};

/// The dashboard snapshot: identity, the global request count, per-ecosystem activity, the driver's
/// counter-family labels, and the configured indexes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UiSnapshot {
    pub version: String,
    pub serial: u64,
    pub requests: u64,
    pub ecosystems: Vec<UiEcosystemSummary>,
    pub families: Vec<UiMetricFamily>,
    pub indexes: Vec<UiIndex>,
}

/// One ecosystem's activity rolled up across its indexes; mirrors peryx's `/+status` summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UiEcosystemSummary {
    pub ecosystem: String,
    pub pages: u64,
    pub downloads: u64,
    pub bytes: u64,
    pub rejected: u64,
    pub uploads: u64,
    pub families: BTreeMap<String, u64>,
}

/// A counter family the ecosystem driver publishes: its storage key and human label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UiMetricFamily {
    pub key: String,
    pub label: String,
    pub roles: Vec<String>,
}

/// One configured index as the dashboard shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiIndex {
    pub name: String,
    pub route: String,
    /// The package ecosystem, for example `pypi`.
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
    pub project_count: u64,
    pub upload_count: u64,
    pub recent_uploads: Vec<UiRecentUpload>,
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

/// One recent upload summary from `/+status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiRecentUpload {
    pub project: String,
    pub filename: String,
    pub version: String,
    pub uploaded_at: Option<String>,
    pub size: Option<u64>,
}

impl UiSnapshot {
    /// Rebuild the snapshot from the `/+status` JSON document.
    #[must_use]
    pub fn from_status(value: &serde_json::Value) -> Self {
        let indexes = value["indexes"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|index| UiIndex {
                name: string_at(index, "name"),
                route: string_at(index, "route"),
                ecosystem: string_at(index, "ecosystem"),
                endpoint: string_at(index, "endpoint"),
                kind: string_at(index, "kind"),
                layers: index["layers"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|layer| layer.as_str().map(str::to_owned))
                    .collect(),
                uploads: index["uploads"].as_bool().unwrap_or(false),
                upload_to: index["upload_to"].as_str().map(str::to_owned),
                upstream: upstream_from_status(index),
                hosted: hosted_from_status(index),
                project_count: u64_at(index, "project_count"),
                upload_count: u64_at(index, "upload_count"),
                recent_uploads: index["recent_uploads"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|upload| UiRecentUpload {
                        project: string_at(upload, "project"),
                        filename: string_at(upload, "filename"),
                        version: string_at(upload, "version"),
                        uploaded_at: upload["uploaded_at"].as_str().map(str::to_owned),
                        size: upload["size"].as_u64(),
                    })
                    .collect(),
            })
            .collect();
        Self {
            version: string_at(value, "version"),
            serial: u64_at(value, "serial"),
            requests: u64_at(value, "requests"),
            ecosystems: serde_json::from_value(value["by_ecosystem"].clone()).unwrap_or_default(),
            families: serde_json::from_value(value["metric_families"].clone()).unwrap_or_default(),
            indexes,
        }
    }
}

fn upstream_from_status(index: &serde_json::Value) -> Option<UiUpstream> {
    let upstream = index["upstream"].as_object()?;
    Some(UiUpstream {
        url: upstream["url"].as_str().unwrap_or_default().to_owned(),
        auth_kind: upstream["auth"]["kind"].as_str().unwrap_or("none").to_owned(),
        auth_redacted: upstream["auth"]["redacted"].as_str().map(str::to_owned),
        status: upstream["status"].as_str().unwrap_or("configured").to_owned(),
    })
}

fn hosted_from_status(index: &serde_json::Value) -> Option<UiHosted> {
    let hosted = index["hosted"].as_object()?;
    Some(UiHosted {
        volatile: hosted["volatile"].as_bool().unwrap_or(false),
        token_configured: hosted["upload_token"]["configured"].as_bool().unwrap_or(false),
        token_redacted: hosted["upload_token"]["redacted"].as_str().map(str::to_owned),
    })
}
