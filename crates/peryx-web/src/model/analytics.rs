use serde::Deserialize;
use serde_json::Value;
use time::format_description::BorrowedFormatItem;
use time::format_description::well_known::Rfc3339;
use time::{Date, OffsetDateTime};

fn date_format() -> Vec<BorrowedFormatItem<'static>> {
    time::format_description::parse_borrowed::<1>("[year]-[month]-[day]").expect("the date format is valid")
}

const SECONDS_PER_DAY: i64 = 86_400;

/// The five read-only usage views the admin page can request, one per `/+analytics/*` endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnalyticsView {
    Top,
    Groups,
    Sources,
    Unused,
    Timeline,
}

impl AnalyticsView {
    /// The persisted selector value, also the round-trip key the filter form carries.
    #[must_use]
    pub const fn as_query(self) -> &'static str {
        match self {
            Self::Top => "top",
            Self::Groups => "groups",
            Self::Sources => "sources",
            Self::Unused => "unused",
            Self::Timeline => "timeline",
        }
    }

    /// Resolve a selector value back to a view, defaulting to the top-resources view.
    #[must_use]
    pub fn from_query(value: &str) -> Self {
        match value {
            "groups" => Self::Groups,
            "sources" => Self::Sources,
            "unused" => Self::Unused,
            "timeline" => Self::Timeline,
            _ => Self::Top,
        }
    }

    /// The API path the view queries.
    #[must_use]
    pub const fn path(self) -> &'static str {
        match self {
            Self::Top => "/+analytics/top-resources",
            Self::Groups => "/+analytics/groups",
            Self::Sources => "/+analytics/sources",
            Self::Unused => "/+analytics/unused",
            Self::Timeline => "/+analytics/timeline",
        }
    }

    /// The array key the view's response envelope stores its rows under.
    const fn rows_key(self) -> &'static str {
        match self {
            Self::Top => "resources",
            Self::Groups => "groups",
            Self::Sources => "sources",
            Self::Unused => "unused",
            Self::Timeline => "buckets",
        }
    }
}

/// The resolved day window a usage query ran over, mirroring the API `interval` envelope.
///
/// `window_clamped_to_retention` is what lets the page tell an empty result apart from one whose
/// requested start predated retention, so a reader never mistakes aged-out data for no activity.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct UiInterval {
    pub from_day: i64,
    pub to_day: i64,
    pub retained_from_day: Option<i64>,
    pub window_clamped_to_retention: bool,
}

impl UiInterval {
    /// The inclusive UTC day window as `YYYY-MM-DD to YYYY-MM-DD`.
    #[must_use]
    pub fn window(&self) -> String {
        format!("{} to {}", format_day(self.from_day), format_day(self.to_day))
    }

    /// The retention floor as a UTC date, present only under a bounded retention policy.
    #[must_use]
    pub fn retained_from(&self) -> Option<String> {
        self.retained_from_day.map(format_day)
    }
}

/// One resource's reads and bytes in the queried window.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct UiResourceRow {
    pub repository: String,
    pub resource: String,
    pub reads: u64,
    pub bytes: u64,
}

/// One resource group's reads; `group` is absent when the ecosystem reported none.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct UiGroupRow {
    pub repository: String,
    pub resource: String,
    pub group: Option<String>,
    pub reads: u64,
    pub bytes: u64,
}

/// One resource's reads attributed to a routed source; `source` is absent for the local store.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct UiSourceRow {
    pub repository: String,
    pub resource: String,
    pub source: Option<String>,
    pub reads: u64,
    pub bytes: u64,
}

/// A resource with lifetime reads but none inside the window; `lifetime_reads` separates an
/// idle resource from one whose activity predates the retained interval.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct UiUnusedRow {
    pub repository: String,
    pub resource: String,
    pub lifetime_reads: u64,
}

/// One UTC-day bucket with half-open `[start_unix, end_unix)` bounds.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct UiTimelineRow {
    pub day: i64,
    pub start_unix: i64,
    pub end_unix: i64,
    pub reads: u64,
    pub bytes: u64,
}

/// The rows of one page, typed by the view that produced them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiUsageRows {
    Top(Vec<UiResourceRow>),
    Groups(Vec<UiGroupRow>),
    Sources(Vec<UiSourceRow>),
    Unused(Vec<UiUnusedRow>),
    Timeline(Vec<UiTimelineRow>),
}

impl UiUsageRows {
    #[must_use]
    pub const fn len(&self) -> usize {
        match self {
            Self::Top(rows) => rows.len(),
            Self::Groups(rows) => rows.len(),
            Self::Sources(rows) => rows.len(),
            Self::Unused(rows) => rows.len(),
            Self::Timeline(rows) => rows.len(),
        }
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// One page of a usage view: its typed rows, the resolved interval, and the next-page cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiUsagePage {
    pub rows: UiUsageRows,
    pub interval: UiInterval,
    pub next_cursor: Option<String>,
}

impl UiUsagePage {
    /// Read one `/+analytics/*` envelope for `view`, returning `None` on any missing or malformed
    /// field so the loader can report invalid data without leaking the response body.
    #[must_use]
    pub fn parse(view: AnalyticsView, value: &Value) -> Option<Self> {
        let interval = serde_json::from_value(value.get("interval")?.clone()).ok()?;
        let next_cursor = match value.get("next_cursor")? {
            Value::Null => None,
            Value::String(cursor) => Some(cursor.clone()),
            _ => return None,
        };
        let array = value.get(view.rows_key())?.clone();
        let rows = match view {
            AnalyticsView::Top => UiUsageRows::Top(serde_json::from_value(array).ok()?),
            AnalyticsView::Groups => UiUsageRows::Groups(serde_json::from_value(array).ok()?),
            AnalyticsView::Sources => UiUsageRows::Sources(serde_json::from_value(array).ok()?),
            AnalyticsView::Unused => UiUsageRows::Unused(serde_json::from_value(array).ok()?),
            AnalyticsView::Timeline => UiUsageRows::Timeline(serde_json::from_value(array).ok()?),
        };
        Some(Self {
            rows,
            interval,
            next_cursor,
        })
    }
}

/// The filter form state, round-tripped across page links so pagination keeps the active query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnalyticsFilters {
    pub view: String,
    pub repository: String,
    pub from: String,
    pub to: String,
    pub limit: String,
}

impl Default for AnalyticsFilters {
    fn default() -> Self {
        Self {
            view: AnalyticsView::Top.as_query().to_owned(),
            repository: String::new(),
            from: String::new(),
            to: String::new(),
            limit: "25".to_owned(),
        }
    }
}

impl AnalyticsFilters {
    #[must_use]
    pub fn view(&self) -> AnalyticsView {
        AnalyticsView::from_query(&self.view)
    }

    /// Build the API URL after validating browser-local date inputs.
    ///
    /// # Errors
    /// Returns a bounded message when either UTC date input cannot be parsed.
    pub fn url(&self, cursor: Option<&str>) -> Result<String, String> {
        let mut serializer = url::form_urlencoded::Serializer::new(String::new());
        let repository = self.repository.trim();
        if !repository.is_empty() {
            serializer.append_pair("repository", repository);
        }
        for (name, value) in [("from", self.from.trim()), ("to", self.to.trim())] {
            if !value.is_empty() {
                serializer.append_pair(name, &parse_date(value)?.to_string());
            }
        }
        serializer.append_pair("limit", self.limit.trim());
        if let Some(cursor) = cursor {
            serializer.append_pair("cursor", cursor);
        }
        Ok(format!("{}?{}", self.view().path(), serializer.finish()))
    }
}

fn parse_date(value: &str) -> Result<i64, String> {
    Date::parse(value, &date_format())
        .map(|date| date.midnight().assume_utc().unix_timestamp())
        .map_err(|_| format!("Invalid UTC date: {value}"))
}

/// Format a day number (days since the Unix epoch) as a `YYYY-MM-DD` UTC date.
fn format_day(day: i64) -> String {
    let Some(timestamp) = day.checked_mul(SECONDS_PER_DAY) else {
        return day.to_string();
    };
    let Ok(time) = OffsetDateTime::from_unix_timestamp(timestamp) else {
        return day.to_string();
    };
    let date = time.date();
    format!("{:04}-{:02}-{:02}", date.year(), u8::from(date.month()), date.day())
}

/// Format a Unix timestamp as an RFC 3339 UTC instant for the timeline bucket bounds.
#[must_use]
pub fn format_instant(value: i64) -> String {
    let Ok(time) = OffsetDateTime::from_unix_timestamp(value) else {
        return value.to_string();
    };
    time.format(&Rfc3339).unwrap_or_else(|_| value.to_string())
}

#[cfg(test)]
#[path = "../../tests/unit/model/analytics/tests.rs"]
mod tests;
