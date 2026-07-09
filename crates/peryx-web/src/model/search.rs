use serde::{Deserialize, Serialize};

use super::{string_at, usize_from};

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
    pub ecosystem: String,
    /// This ecosystem's word for the result (`package`, `image`), filled server-side from the lexicon.
    pub type_label: String,
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
                    ecosystem: string_at(result, "ecosystem"),
                    type_label: string_at(result, "type_label"),
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
