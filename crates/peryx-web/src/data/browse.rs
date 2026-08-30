use peryx_core::BrowsePage;
#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
use peryx_core::UiActionMethod;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum LoaderEndpoint {
    Browse,
    Session,
    Stats,
    Status,
    Topology,
}

impl std::fmt::Display for LoaderEndpoint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Browse => "/+ui/browse",
            Self::Session => "/_/session",
            Self::Stats => "/+stats",
            Self::Status => "/+status",
            Self::Topology => "/+availability/topology",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum LoaderError {
    Request(LoaderEndpoint),
    Status { endpoint: LoaderEndpoint, status: u16 },
    Invalid(LoaderEndpoint),
}

impl std::fmt::Display for LoaderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Request(endpoint) => write!(formatter, "Request to {endpoint} failed."),
            Self::Status { endpoint, status } => write!(formatter, "{endpoint} returned HTTP {status}."),
            Self::Invalid(endpoint) => write!(formatter, "{endpoint} returned invalid data."),
        }
    }
}

impl std::error::Error for LoaderError {}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
pub(super) async fn fetch_json_required<T>(url: &str, endpoint: LoaderEndpoint) -> Result<T, LoaderError>
where
    T: serde::de::DeserializeOwned,
{
    let Some(value) = fetch_json_optional(url, endpoint).await? else {
        return Err(LoaderError::Status { endpoint, status: 404 });
    };
    Ok(value)
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
pub(super) async fn fetch_json_optional<T>(url: &str, endpoint: LoaderEndpoint) -> Result<Option<T>, LoaderError>
where
    T: serde::de::DeserializeOwned,
{
    send_wrapper::SendWrapper::new(async {
        let response = gloo_net::http::Request::get(url)
            .header("accept", "application/json")
            .send()
            .await
            .map_err(|_| LoaderError::Request(endpoint))?;
        if response.status() == 404 {
            return Ok(None);
        }
        if !response.ok() {
            return Err(LoaderError::Status {
                endpoint,
                status: response.status(),
            });
        }
        response
            .json()
            .await
            .map(Some)
            .map_err(|_| LoaderError::Invalid(endpoint))
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
            let Some(page) =
                super::fetch_json_optional(&crate::url::ui_browse_url(&raw_query), super::LoaderEndpoint::Browse)
                    .await
                    .map_err(|error| error.to_string())?
            else {
                return Ok(None);
            };
            Ok(Some(page))
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
