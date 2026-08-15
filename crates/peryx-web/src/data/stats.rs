#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
use crate::model::{AnalyticsView, UiUsagePage};

pub async fn load_stats(index: Option<String>, resource: Option<String>) -> crate::model::UiStats {
    #[cfg(feature = "ssr")]
    {
        parse_stats(
            &crate::ssr::stats(index.as_deref(), resource.as_deref()).await,
            index.as_deref(),
            resource.as_deref(),
        )
    }
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    {
        send_wrapper::SendWrapper::new(async move {
            super::fetch_json(&crate::url::stats_api_url(index.as_deref(), resource.as_deref()))
                .await
                .map_or_else(Default::default, |value| {
                    parse_stats(&value, index.as_deref(), resource.as_deref())
                })
        })
        .await
    }
    #[cfg(all(not(feature = "ssr"), not(feature = "hydrate")))]
    {
        let _ = (index, resource);
        crate::model::UiStats::default()
    }
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
/// # Errors
///
/// Returns an error when the service rejects the request or returns invalid data.
pub async fn load_analytics(url: &str, view: AnalyticsView, user: &str, password: &str) -> Result<UiUsagePage, String> {
    send_wrapper::SendWrapper::new(async move {
        use base64::Engine as _;

        let credentials = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
        let response = gloo_net::http::Request::get(url)
            .header("accept", "application/json")
            .header("authorization", &format!("Basic {credentials}"))
            .send()
            .await
            .map_err(|_| "The analytics service could not be reached.".to_owned())?;
        match response.status() {
            200 => {
                let value = response
                    .json()
                    .await
                    .map_err(|_| "The analytics service returned invalid data.".to_owned())?;
                UiUsagePage::parse(view, &value)
                    .ok_or_else(|| "The analytics service returned invalid data.".to_owned())
            }
            400 => Err("One or more analytics filters are invalid.".to_owned()),
            401 => Err("The username or password was not accepted.".to_owned()),
            403 => Err("This repository token cannot inspect usage analytics.".to_owned()),
            404 => Err("The repository was not found or is not available to this user.".to_owned()),
            _ => Err("The analytics service is unavailable.".to_owned()),
        }
    })
    .await
}

#[cfg(any(feature = "ssr", feature = "hydrate"))]
fn parse_stats(value: &serde_json::Value, index: Option<&str>, resource: Option<&str>) -> crate::model::UiStats {
    match (index, resource) {
        (Some(_), Some(_)) => crate::model::stats_resource(value),
        (Some(_), None) => crate::model::stats_index(value),
        (None, _) => crate::model::stats_routes(value),
    }
}

#[cfg(all(test, any(feature = "ssr", feature = "hydrate")))]
#[path = "../../tests/unit/data/stats/tests.rs"]
mod tests;
