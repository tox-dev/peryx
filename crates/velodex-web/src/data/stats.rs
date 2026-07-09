#![allow(
    clippy::future_not_send,
    reason = "browser fetch futures are single-threaded by nature; callers wrap them in SendWrapper"
)]

/// The stats drill at the requested depth: all indexes, one index's projects, or one project's
/// files.
pub async fn load_stats(index: Option<String>, project: Option<String>) -> crate::model::UiStats {
    #[cfg(feature = "ssr")]
    {
        parse_stats(
            &crate::ssr::stats(index.as_deref(), project.as_deref()),
            index.as_deref(),
            project.as_deref(),
        )
    }
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    {
        send_wrapper::SendWrapper::new(async move {
            super::fetch_json(&crate::url::stats_api_url(index.as_deref(), project.as_deref()))
                .await
                .map_or_else(Default::default, |value| {
                    parse_stats(&value, index.as_deref(), project.as_deref())
                })
        })
        .await
    }
    #[cfg(all(not(feature = "ssr"), not(feature = "hydrate")))]
    {
        let _ = (index, project);
        crate::model::UiStats::default()
    }
}

#[cfg(any(feature = "ssr", feature = "hydrate"))]
fn parse_stats(value: &serde_json::Value, index: Option<&str>, project: Option<&str>) -> crate::model::UiStats {
    match (index, project) {
        (Some(_), Some(_)) => crate::model::stats_project(value),
        (Some(_), None) => crate::model::stats_index(value),
        (None, _) => crate::model::stats_routes(value),
    }
}
