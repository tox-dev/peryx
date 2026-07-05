//! Serializable view models shared by the server renderer and the hydrated client.
//!
//! The server builds them from `AppState`; the browser rebuilds them from velodex's own JSON API
//! (`/+status` and the PEP 691 simple endpoints), so both sides render identical pages.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

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

/// One ecosystem's activity rolled up across its indexes; mirrors velodex's `/+status` summary.
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

/// A project page: the files of one project on one index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UiProject {
    pub name: String,
    pub versions: Vec<String>,
    pub files: Vec<UiFile>,
}

/// One downloadable file as the project page shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiFile {
    pub filename: String,
    pub url: String,
    pub sha256: String,
    pub size: Option<u64>,
    pub upload_time: Option<String>,
    pub yanked: bool,
    pub has_metadata: bool,
}

/// One member of a distribution archive, as the archive browser lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiMember {
    pub path: String,
    pub size: u64,
    pub kind: String,
    pub previewable: bool,
}

/// One rendered chunk of an archive member.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UiMemberChunk {
    pub text: String,
    pub size: Option<u64>,
    pub offset: u64,
    pub next_offset: Option<u64>,
}

/// Rebuild an archive listing from the inspect endpoint's JSON document.
#[must_use]
pub fn members_from_listing(value: &serde_json::Value) -> Vec<UiMember> {
    value["members"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|member| UiMember {
            path: string_at(member, "path"),
            size: member["size"].as_u64().unwrap_or_default(),
            kind: member["kind"].as_str().unwrap_or("unknown").to_owned(),
            previewable: member["previewable"].as_bool().unwrap_or(false),
        })
        .collect()
}

impl UiProject {
    /// Rebuild a project page from a PEP 691 project-detail document.
    #[must_use]
    pub fn from_detail(value: &serde_json::Value) -> Self {
        let files = value["files"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|file| UiFile {
                filename: string_at(file, "filename"),
                url: string_at(file, "url"),
                sha256: file["hashes"]["sha256"].as_str().unwrap_or_default().to_owned(),
                size: file["size"].as_u64(),
                upload_time: file["upload-time"].as_str().map(str::to_owned),
                yanked: file["yanked"].as_bool().unwrap_or(false) || file["yanked"].is_string(),
                has_metadata: file["core-metadata"].is_object() || file["core-metadata"].as_bool() == Some(true),
            })
            .collect();
        Self {
            name: string_at(value, "name"),
            versions: value["versions"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|version| version.as_str().map(str::to_owned))
                .collect(),
            files,
        }
    }
}

/// The project names of one index, from its PEP 691 list document.
#[must_use]
pub fn projects_from_list(value: &serde_json::Value) -> Vec<String> {
    value["projects"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|project| string_at(project, "name"))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UiSearchPage {
    pub query: String,
    pub source_type: String,
    pub page: usize,
    pub page_size: usize,
    pub total: usize,
    pub results: Vec<UiSearchResult>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiSearchResult {
    pub display_name: String,
    pub normalized_name: String,
    pub route: String,
    pub index: String,
    pub source_type: String,
    pub summary: Option<String>,
}

impl UiSearchPage {
    #[must_use]
    pub fn from_search(value: &serde_json::Value) -> Self {
        Self {
            query: string_at(value, "query"),
            source_type: string_at(value, "type"),
            page: usize_from(value["page"].as_u64(), 1),
            page_size: usize_from(value["page_size"].as_u64(), 25),
            total: usize_from(value["total"].as_u64(), 0),
            results: value["results"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|result| UiSearchResult {
                    display_name: string_at(result, "display_name"),
                    normalized_name: string_at(result, "normalized_name"),
                    route: string_at(result, "route"),
                    index: string_at(result, "index"),
                    source_type: string_at(result, "type"),
                    summary: result["summary"].as_str().map(str::to_owned),
                })
                .collect(),
        }
    }
}

impl UiSearchResult {
    #[must_use]
    pub fn source_label(&self) -> &'static str {
        source_label(&self.source_type)
    }
}

#[must_use]
pub fn source_label(source_type: &str) -> &'static str {
    match source_type {
        "uploaded" => "Uploaded",
        "override" => "Override",
        _ => "Cached",
    }
}

fn string_at(value: &serde_json::Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().to_owned()
}

fn u64_at(value: &serde_json::Value, key: &str) -> u64 {
    value[key].as_u64().unwrap_or_default()
}

fn usize_from(value: Option<u64>, default: usize) -> usize {
    value.and_then(|value| usize::try_from(value).ok()).unwrap_or(default)
}

/// Usage counters at one aggregation level of `/+stats`. File-level entries fill only the fields
/// the server tracks per file (downloads, metadata, bytes); the rest stay zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UiCounters {
    pub pages: u64,
    pub downloads: u64,
    pub metadata: u64,
    pub uploads: u64,
    pub bytes: u64,
    pub refreshes: u64,
    pub changed: u64,
    pub stale_served: u64,
    pub upstream_errors: u64,
    pub rejected: u64,
}

impl UiCounters {
    /// Read the counters present in one stats JSON object; absent fields stay zero. Index and
    /// project totals group counters by owning role (`base`/`cached`/`hosted`/`ecosystem`); file
    /// entries carry `downloads`/`bytes` flat and their ecosystem counters under `ecosystem`, so
    /// each field falls back to the flat location.
    #[must_use]
    pub fn from_value(value: &serde_json::Value) -> Self {
        Self {
            pages: grouped(value, "base", "pages"),
            downloads: grouped(value, "base", "downloads"),
            metadata: grouped(value, "ecosystem", "metadata"),
            uploads: grouped(value, "hosted", "uploads"),
            bytes: grouped(value, "base", "bytes"),
            refreshes: grouped(value, "cached", "refreshes"),
            changed: grouped(value, "cached", "changed"),
            stale_served: grouped(value, "cached", "stale_served"),
            upstream_errors: grouped(value, "cached", "upstream_errors"),
            rejected: grouped(value, "base", "rejected"),
        }
    }
}

/// Read `value[group][field]`, falling back to a flat `value[field]` for file-level entries.
fn grouped(value: &serde_json::Value, group: &str, field: &str) -> u64 {
    value
        .get(group)
        .and_then(|group| group.get(field))
        .or_else(|| value.get(field))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
}

/// One drill depth of `/+stats`: the aggregate at this level plus the named rows underneath
/// (indexes, then projects, then files), busiest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UiStats {
    pub totals: UiCounters,
    pub rows: Vec<(String, UiCounters)>,
}

fn sorted_rows(value: &serde_json::Value) -> Vec<(String, UiCounters)> {
    let mut rows: Vec<(String, UiCounters)> = value
        .as_object()
        .into_iter()
        .flatten()
        .map(|(name, counters)| (name.clone(), UiCounters::from_value(counters)))
        .collect();
    rows.sort_by(|(a_name, a), (b_name, b)| (b.downloads + b.pages, a_name).cmp(&(a.downloads + a.pages, b_name)));
    rows
}

/// Parse the top-level `/+stats` document: one row per index route, totals summed across them.
#[must_use]
pub fn stats_routes(value: &serde_json::Value) -> UiStats {
    let rows = sorted_rows(value);
    let mut totals = UiCounters::default();
    for (_, counters) in &rows {
        totals.pages += counters.pages;
        totals.downloads += counters.downloads;
        totals.metadata += counters.metadata;
        totals.uploads += counters.uploads;
        totals.bytes += counters.bytes;
        totals.refreshes += counters.refreshes;
        totals.changed += counters.changed;
        totals.stale_served += counters.stale_served;
        totals.upstream_errors += counters.upstream_errors;
        totals.rejected += counters.rejected;
    }
    UiStats { totals, rows }
}

/// Parse one index's drill document: its totals plus one row per project.
#[must_use]
pub fn stats_index(value: &serde_json::Value) -> UiStats {
    UiStats {
        totals: UiCounters::from_value(&value["totals"]),
        rows: sorted_rows(&value["projects"]),
    }
}

/// Parse one project's drill document: its totals plus one row per file.
#[must_use]
pub fn stats_project(value: &serde_json::Value) -> UiStats {
    UiStats {
        totals: UiCounters::from_value(&value["totals"]),
        rows: sorted_rows(&value["files"]),
    }
}
