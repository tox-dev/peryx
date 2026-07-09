use serde::{Deserialize, Serialize};

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
