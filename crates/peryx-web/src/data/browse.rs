use peryx_core::BrowsePage;
#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
use peryx_core::UiActionMethod;

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
pub(super) async fn fetch_json(url: &str) -> Option<serde_json::Value> {
    fetch_json_required(url).await.ok()
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
pub(super) async fn fetch_json_required(url: &str) -> Result<serde_json::Value, String> {
    let Some(value) = fetch_json_optional(url).await? else {
        return Err(format!("404 from {url}: not found"));
    };
    Ok(value)
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
pub(super) async fn fetch_json_optional(url: &str) -> Result<Option<serde_json::Value>, String> {
    send_wrapper::SendWrapper::new(async {
        let response = gloo_net::http::Request::get(url)
            .header("accept", "application/json")
            .send()
            .await
            .map_err(|error| format!("request failed for {url}: {error}"))?;
        if response.status() == 404 {
            return Ok(None);
        }
        if !response.ok() {
            return Err(response_error(response, url).await);
        }
        response
            .json()
            .await
            .map(Some)
            .map_err(|error| format!("invalid JSON from {url}: {error}"))
    })
    .await
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
async fn response_error(response: gloo_net::http::Response, url: &str) -> String {
    send_wrapper::SendWrapper::new(async move {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if text.is_empty() {
            format!("{status} from {url}")
        } else {
            format!("{status} from {url}: {text}")
        }
    })
    .await
}

/// # Errors
///
/// Returns an error when browse resolution fails or the response is invalid.
pub async fn load_browse(raw_query: String) -> Result<Option<BrowsePage>, String> {
    #[cfg(feature = "ssr")]
    {
        crate::ssr::browse(&raw_query).await
    }
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    {
        send_wrapper::SendWrapper::new(async move {
            let Some(value) = super::fetch_json_optional(&crate::url::ui_browse_url(&raw_query)).await? else {
                return Ok(None);
            };
            serde_json::from_value(value)
                .map(Some)
                .map_err(|err| format!("invalid browse response: {err}"))
        })
        .await
    }
    #[cfg(all(not(feature = "ssr"), not(feature = "hydrate")))]
    {
        let _ = raw_query;
        Ok(None)
    }
}

/// Send an authenticated plugin action and return its status and response body.
#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
pub async fn admin_request(method: UiActionMethod, url: &str, user: &str, password: &str) -> String {
    send_wrapper::SendWrapper::new(async move {
        use base64::Engine as _;

        let credentials = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
        let request = match method {
            UiActionMethod::Put => gloo_net::http::Request::put(url),
            UiActionMethod::Post => gloo_net::http::Request::post(url),
            UiActionMethod::Delete => gloo_net::http::Request::delete(url),
        };
        match request
            .header("authorization", &format!("Basic {credentials}"))
            .send()
            .await
        {
            Ok(response) => {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                format!("{status}: {body}")
            }
            Err(err) => format!("request failed: {err}"),
        }
    })
    .await
}
