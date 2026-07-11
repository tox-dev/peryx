use std::sync::Arc;

use leptos::prelude::*;
use peryx_driver::AppState;
use peryx_search::{SearchParams, SourceFilter};

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
        .search(&app.search_ctx(), params)
        .map_err(|err| format!("package search: {err}"))?;
    let value = serde_json::to_value(response).map_err(|err| format!("search result: {err}"))?;
    Ok(UiSearchPage::from_search(&value))
}
