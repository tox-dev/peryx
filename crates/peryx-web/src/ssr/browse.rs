use std::sync::Arc;

use leptos::prelude::*;
use peryx_core::BrowsePage;
use peryx_driver::AppState;

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
    if !app.serving.index_at(position).acl.anonymous_read
        && super::read_access(&app.serving)
            .await?
            .for_index(app.serving.index_at(position))
            .authorize_any_resource()
            .is_err()
    {
        return Err("read access denied".to_owned());
    }
    let Some(browse) = app.driver_set().get_browse(&app.serving.index_at(position).ecosystem) else {
        return Err(format!("index {route:?} does not support browsing"));
    };
    browse.browse(app.serving.clone(), position, raw_query.to_owned()).await
}
