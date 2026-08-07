//! The search result documents returned to callers.

use serde::{Deserialize, Serialize};

use crate::params::{AvailabilityFilter, PackageSource, SourceFilter};

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
    pub display_name: String,
    pub normalized_name: String,
    pub route: String,
    pub index: String,
    /// The index's ecosystem, so a surface can label the result in that ecosystem's own words.
    pub ecosystem: String,
    /// That ecosystem's word for a searchable collection, resolved before serialization.
    pub type_label: String,
    #[serde(rename = "type")]
    pub source_type: PackageSource,
    /// Whether this package can be served from local storage right now, so a surface can flag it
    /// without a second lookup.
    #[serde(rename = "available")]
    pub available_locally: bool,
    pub summary: Option<String>,
}
