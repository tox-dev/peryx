use peryx_core::{BlobPlacementView, PlacementView};

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
use crate::model::UiTrashPage;

/// The artifact placement-health view, projected to the caller's class.
///
/// The server reads and projects `AppState`; the hydrated browser fetches `/+availability/placements`,
/// which projects the same fields, so both sides yield the identical [`PlacementView`]. `cursor` pages
/// the administrator's rows in digest order; an operator reads only the aggregate and ignores it.
///
/// # Errors
///
/// Returns a message when the view cannot be reached, access is denied, or the response does not parse.
pub async fn load_placements(cursor: Option<String>) -> Result<PlacementView, String> {
    #[cfg(feature = "ssr")]
    {
        let _ = cursor;
        crate::ssr::placements().await
    }
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    {
        send_wrapper::SendWrapper::new(async move {
            // Digests are `sha256:` plus hex, so they carry no character a query value must escape.
            let url = cursor.map_or_else(
                || "/+availability/placements".to_owned(),
                |cursor| format!("/+availability/placements?cursor={cursor}"),
            );
            let response = gloo_net::http::Request::get(&url)
                .header("accept", "application/json")
                .send()
                .await
                .map_err(|_| "Placement health could not be reached.".to_owned())?;
            match response.status() {
                200 => response
                    .json()
                    .await
                    .map_err(|_| "Placement health returned invalid data.".to_owned()),
                400 => Err("The placement page request was invalid.".to_owned()),
                401 | 403 => Err("You do not have access to placement health.".to_owned()),
                _ => Err("Placement health is unavailable.".to_owned()),
            }
        })
        .await
    }
    #[cfg(all(not(feature = "ssr"), not(feature = "hydrate")))]
    {
        let _ = cursor;
        Err("Placement health is unavailable.".to_owned())
    }
}

/// Where one blob's bytes are placed across datacenters, for an administrator drilling into a digest.
///
/// # Errors
///
/// Returns a message when the view cannot be reached, access is denied, or the response does not parse.
pub async fn load_blob_placement(digest: String) -> Result<BlobPlacementView, String> {
    #[cfg(feature = "ssr")]
    {
        crate::ssr::blob_placements(digest).await
    }
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    {
        send_wrapper::SendWrapper::new(async move {
            // A digest is `sha256:` plus hex, which carries no character a path segment must escape.
            let response = gloo_net::http::Request::get(&format!("/+availability/placements/{digest}"))
                .header("accept", "application/json")
                .send()
                .await
                .map_err(|_| "Blob placement could not be reached.".to_owned())?;
            match response.status() {
                200 => response
                    .json()
                    .await
                    .map_err(|_| "Blob placement returned invalid data.".to_owned()),
                400 => Err("That is not a valid artifact digest.".to_owned()),
                401 | 403 => Err("You do not have access to blob placement.".to_owned()),
                _ => Err("Blob placement is unavailable.".to_owned()),
            }
        })
        .await
    }
    #[cfg(all(not(feature = "ssr"), not(feature = "hydrate")))]
    {
        let _ = digest;
        Err("Blob placement is unavailable.".to_owned())
    }
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
/// # Errors
///
/// Returns an error when the service rejects the request or returns invalid data.
pub async fn load_trash(url: &str, user: &str, password: &str) -> Result<UiTrashPage, String> {
    send_wrapper::SendWrapper::new(async move {
        use base64::Engine as _;

        let credentials = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
        let response = gloo_net::http::Request::get(url)
            .header("accept", "application/json")
            .header("authorization", &format!("Basic {credentials}"))
            .send()
            .await
            .map_err(|_| "The trash inspection service could not be reached.".to_owned())?;
        match response.status() {
            200 => response
                .json()
                .await
                .map_err(|_| "The trash inspection service returned invalid data.".to_owned()),
            400 => Err("One or more trash filters are invalid.".to_owned()),
            401 => Err("The username or password was not accepted.".to_owned()),
            403 => Err("This token cannot inspect trash.".to_owned()),
            404 => Err("The repository was not found or is not available to this user.".to_owned()),
            _ => Err("The trash inspection service is unavailable.".to_owned()),
        }
    })
    .await
}
