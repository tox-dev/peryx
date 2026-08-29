use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, OriginalUri};
use axum::http::HeaderMap;
use leptos::prelude::*;
use peryx_core::BrowsePage;
use peryx_driver::discovery::BaseUrl;
use peryx_driver::serving::BrowseRequest;
use peryx_driver::{AppState, ServingState};

use super::resolve;

/// # Errors
///
/// Returns an error when index resolution, authorization, or ecosystem browsing fails.
pub async fn browse(raw_query: &str) -> Result<Option<BrowsePage>, String> {
    let app = expect_context::<Arc<AppState>>();
    let mut route = String::new();
    for (key, value) in url::form_urlencoded::parse(raw_query.as_bytes()) {
        if key == "index" {
            route = value.into_owned();
        }
    }
    let (position, _) = resolve(&app, &route)?;
    let Some(browse) = app.driver_set().get_browse(&app.serving.index_at(position).ecosystem) else {
        return Err(format!("index {route:?} does not support browsing"));
    };
    let access = if app.serving.index_at(position).acl.anonymous_read {
        peryx_driver::access::ReadAccess::from_headers(&app.serving, &HeaderMap::new())
    } else {
        super::read_access(&app.serving).await?
    };
    let base = request_base(&app.serving).await;
    browse
        .browse(BrowseRequest {
            state: app.serving.clone(),
            position,
            raw_query: raw_query.to_owned(),
            access: &access,
            base: base.as_ref(),
        })
        .await
        .map_err(|error| error.to_string())
}

async fn request_base(state: &ServingState) -> Option<BaseUrl> {
    let headers = leptos_axum::extract::<HeaderMap>().await.ok()?;
    let OriginalUri(uri) = leptos_axum::extract::<OriginalUri>().await.ok()?;
    let trusted_proxy = leptos_axum::extract::<ConnectInfo<SocketAddr>>()
        .await
        .ok()
        .is_some_and(|ConnectInfo(address)| state.rate_limits.trusts_proxy(address.ip()));
    BaseUrl::from_request(&headers, &uri, trusted_proxy)
}
