//! Serializable view models shared by the server renderer and the hydrated client.
//!
//! The server builds them from `AppState`; the browser rebuilds them from velodex's own JSON API
//! (`/+status` and the PEP 691 simple endpoints), so both sides render identical pages.

use serde::{Deserialize, Serialize};

/// The dashboard snapshot: identity, counters, and the configured indexes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UiSnapshot {
    pub version: String,
    pub serial: u64,
    pub requests: u64,
    pub metadata_requests: u64,
    pub indexes: Vec<UiIndex>,
}

/// One configured index as the dashboard shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiIndex {
    pub name: String,
    pub route: String,
    /// `mirror`, `local`, or `overlay`.
    pub kind: String,
    /// Layer names for an overlay; empty otherwise.
    pub layers: Vec<String>,
    /// Whether uploads are enabled (a local layer with a token).
    pub uploads: bool,
    /// For an overlay: the layer uploads land in.
    pub upload_to: Option<String>,
    pub upstream: Option<UiUpstream>,
    pub local: Option<UiLocal>,
    pub project_count: u64,
    pub upload_count: u64,
    pub recent_uploads: Vec<UiRecentUpload>,
}

/// Mirror status data with credential material redacted by the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiUpstream {
    pub url: String,
    pub auth_kind: String,
    pub auth_redacted: Option<String>,
    pub status: String,
}

/// Local status data with upload-token values redacted by the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiLocal {
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
                local: local_from_status(index),
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
            metadata_requests: u64_at(value, "metadata_requests"),
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

fn local_from_status(index: &serde_json::Value) -> Option<UiLocal> {
    let local = index["local"].as_object()?;
    Some(UiLocal {
        volatile: local["volatile"].as_bool().unwrap_or(false),
        token_configured: local["upload_token"]["configured"].as_bool().unwrap_or(false),
        token_redacted: local["upload_token"]["redacted"].as_str().map(str::to_owned),
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
    pub repository: String,
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
                    repository: string_at(result, "repository"),
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
        "hosted" => "Hosted",
        "upstream-overrides" => "Upstream+",
        _ => "Upstream",
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
    /// Read the counters present in one stats JSON object; absent fields stay zero.
    #[must_use]
    pub fn from_value(value: &serde_json::Value) -> Self {
        Self {
            pages: u64_at(value, "pages"),
            downloads: u64_at(value, "downloads"),
            metadata: u64_at(value, "metadata"),
            uploads: u64_at(value, "uploads"),
            bytes: u64_at(value, "bytes"),
            refreshes: u64_at(value, "refreshes"),
            changed: u64_at(value, "changed"),
            stale_served: u64_at(value, "stale_served"),
            upstream_errors: u64_at(value, "upstream_errors"),
            rejected: u64_at(value, "rejected"),
        }
    }
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

/// Parse one project's drill document: its (flattened) totals plus one row per file.
#[must_use]
pub fn stats_project(value: &serde_json::Value) -> UiStats {
    UiStats {
        totals: UiCounters::from_value(value),
        rows: sorted_rows(&value["files"]),
    }
}
