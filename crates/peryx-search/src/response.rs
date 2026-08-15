use serde::{Deserialize, Serialize};

use crate::params::{AvailabilityFilter, ContentSource, SourceFilter};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SearchResponse {
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    #[serde(rename = "type")]
    pub source_type: SourceFilter,
    pub availability: AvailabilityFilter,
    pub page: usize,
    pub page_size: usize,
    pub total: usize,
    pub results: Vec<SearchResult>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchResult {
    pub display_label: String,
    pub resource_key: String,
    pub route: String,
    pub index: String,
    pub ecosystem: String,
    pub type_label: String,
    #[serde(rename = "type")]
    pub source_type: ContentSource,
    /// Precomputed to avoid a second storage lookup.
    #[serde(rename = "available")]
    pub available_locally: bool,
    pub summary: Option<String>,
}
