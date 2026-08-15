use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UiSearchPage {
    pub query: String,
    #[serde(rename = "type")]
    pub source_type: String,
    pub availability: String,
    pub page: usize,
    pub page_size: usize,
    pub total: usize,
    pub results: Vec<UiSearchResult>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiSearchResult {
    pub display_label: String,
    pub resource_key: String,
    pub route: String,
    pub index: String,
    pub ecosystem: String,
    /// The owner-provided label for this result.
    pub type_label: String,
    #[serde(rename = "type")]
    pub source_type: String,
    /// Whether this result's bytes can be served from local storage now.
    pub available: bool,
    pub summary: Option<String>,
}

impl UiSearchPage {
    /// The 1-based inclusive `(start, end)` row interval this page shows in its summary, or `None`
    /// when the page holds no rows. A page requested past the last result carries a nonzero total
    /// yet an empty vector, and its start would otherwise run beyond both the end and the total.
    #[must_use]
    pub fn shown_range(&self) -> Option<(usize, usize)> {
        let last = self.results.len().checked_sub(1)?;
        let start = self.page.saturating_sub(1).saturating_mul(self.page_size) + 1;
        Some((start, self.total.min(start + last)))
    }
}

#[cfg(feature = "ssr")]
impl From<peryx_search::SearchResponse> for UiSearchPage {
    fn from(response: peryx_search::SearchResponse) -> Self {
        Self {
            query: response.query,
            source_type: response.source_type.as_str().to_owned(),
            availability: response.availability.as_str().to_owned(),
            page: response.page,
            page_size: response.page_size,
            total: response.total,
            results: response
                .results
                .into_iter()
                .map(|result| UiSearchResult {
                    display_label: result.display_label,
                    resource_key: result.resource_key,
                    route: result.route,
                    index: result.index,
                    ecosystem: result.ecosystem,
                    type_label: result.type_label,
                    source_type: result.source_type.as_str().to_owned(),
                    available: result.available_locally,
                    summary: result.summary,
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
