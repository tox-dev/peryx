use std::sync::Arc;

use leptos::prelude::*;
use peryx_http::AppState;
use peryx_http::search::{SearchParams, SourceFilter};

use crate::model::UiSearchPage;

/// Search cached packages during server rendering.
///
/// # Errors
/// Returns a user-visible message when search fails.
pub fn search(query: &str, source_type: &str, page: usize, page_size: usize) -> Result<UiSearchPage, String> {
    let app = expect_context::<Arc<AppState>>();
    let params = SearchParams {
        query: query.to_owned(),
        route: None,
        source: SourceFilter::from_value(source_type).unwrap_or(SourceFilter::All),
        page: page.max(1),
        page_size: match page_size {
            25 | 50 | 100 => page_size,
            _ => 25,
        },
    };
    let response = app
        .search
        .search(&app, params)
        .map_err(|err| format!("package search: {err}"))?;
    let value = serde_json::to_value(response).map_err(|err| format!("search result: {err}"))?;
    Ok(UiSearchPage::from_search(&value))
}

/// The repositories an OCI index holds, from the search index scoped to that route.
///
/// # Errors
/// Returns a user-visible message when the search index cannot be read.
pub fn repositories(route: &str) -> Result<Vec<String>, String> {
    let app = expect_context::<Arc<AppState>>();
    let params = SearchParams {
        query: String::new(),
        route: Some(route.to_owned()),
        source: SourceFilter::All,
        page: 1,
        page_size: 100,
    };
    let response = app
        .search
        .search(&app, params)
        .map_err(|err| format!("repository list on index {route:?}: {err}"))?;
    Ok(response.results.into_iter().map(|result| result.display_name).collect())
}
