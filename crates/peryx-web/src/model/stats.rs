use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UiCounters {
    pub pages: u64,
    pub reads: u64,
    pub metadata: u64,
    pub writes: u64,
    pub bytes: u64,
    pub refreshes: u64,
    pub changed: u64,
    pub stale_served: u64,
    pub upstream_errors: u64,
    pub rejected: u64,
}

impl UiCounters {
    #[must_use]
    pub fn from_value(value: &serde_json::Value) -> Self {
        Self {
            pages: grouped(value, "base", "pages"),
            reads: grouped(value, "base", "reads"),
            metadata: grouped(value, "ecosystem", "metadata"),
            writes: grouped(value, "hosted", "writes"),
            bytes: grouped(value, "base", "bytes"),
            refreshes: grouped(value, "cached", "refreshes"),
            changed: grouped(value, "cached", "changed"),
            stale_served: grouped(value, "cached", "stale_served"),
            upstream_errors: grouped(value, "cached", "upstream_errors"),
            rejected: grouped(value, "base", "rejected"),
        }
    }
}

fn grouped(value: &serde_json::Value, group: &str, field: &str) -> u64 {
    value
        .get(group)
        .and_then(|group| group.get(field))
        .or_else(|| value.get(field))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
}

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
    rows.sort_by(|(a_name, a), (b_name, b)| activity(b).cmp(&activity(a)).then_with(|| a_name.cmp(b_name)));
    rows
}

const fn activity(counters: &UiCounters) -> u64 {
    counters.reads + counters.pages
}

/// Parse the top-level `/+stats` document: one row per index route, totals summed across them.
#[must_use]
pub fn stats_routes(value: &serde_json::Value) -> UiStats {
    let rows = sorted_rows(value);
    let mut totals = UiCounters::default();
    for (_, counters) in &rows {
        totals.pages += counters.pages;
        totals.reads += counters.reads;
        totals.metadata += counters.metadata;
        totals.writes += counters.writes;
        totals.bytes += counters.bytes;
        totals.refreshes += counters.refreshes;
        totals.changed += counters.changed;
        totals.stale_served += counters.stale_served;
        totals.upstream_errors += counters.upstream_errors;
        totals.rejected += counters.rejected;
    }
    UiStats { totals, rows }
}

#[must_use]
pub fn stats_index(value: &serde_json::Value) -> UiStats {
    UiStats {
        totals: UiCounters::from_value(&value["totals"]),
        rows: sorted_rows(&value["resources"]),
    }
}

#[must_use]
pub fn stats_resource(value: &serde_json::Value) -> UiStats {
    UiStats {
        totals: UiCounters::from_value(&value["totals"]),
        rows: sorted_rows(&value["artifacts"]),
    }
}
