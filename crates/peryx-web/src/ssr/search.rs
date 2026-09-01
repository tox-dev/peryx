use std::sync::Arc;

use leptos::prelude::*;
use peryx_driver::AppState;
use peryx_driver::http_services::HttpDomainServices;
use peryx_search::{AvailabilityFilter, SearchParams, SourceFilter};

use crate::model::UiSearchPage;

/// Search cached artifacts during server rendering.
///
/// # Errors
/// Returns a user-visible message when search fails.
pub async fn search(
    query: &str,
    source_type: &str,
    availability: &str,
    page: usize,
    page_size: usize,
) -> Result<UiSearchPage, String> {
    let app = expect_context::<Arc<AppState>>();
    let params = SearchParams {
        query: query.to_owned(),
        route: None,
        source: SourceFilter::from_value(source_type).unwrap_or(SourceFilter::All),
        availability: AvailabilityFilter::from_value(availability).unwrap_or(AvailabilityFilter::All),
        // The browser surface authenticates nobody, so it never carries pattern authority.
        pattern_authority: false,
        page: page.max(1),
        page_size: match page_size {
            25 | 50 | 100 => page_size,
            _ => 25,
        },
    };
    let access = if app.serving.indexes.iter().all(|index| index.acl.anonymous_read) {
        None
    } else {
        Some(
            super::read_access(&app.serving)
                .await?
                .search_access(&app.serving.indexes),
        )
    };
    let response = peryx_http::handlers::search_offloaded(
        &app.blocking_scans,
        HttpDomainServices::for_state(&app),
        params,
        access,
    )
    .await
    .map_err(|error| format!("artifact search: {error}"))?;
    Ok(UiSearchPage::from(response))
}
