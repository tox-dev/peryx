//! Data loading for the UI, compiled per side: the server reads `AppState` directly, the hydrated
//! browser fetches velodex's own JSON API. Both produce the same view models.
#![allow(
    clippy::future_not_send,
    reason = "browser fetch futures are single-threaded by nature; callers wrap them in SendWrapper"
)]

use velodex_ecosystem_pypi::CoreMetadataDoc;

use crate::model::{UiMember, UiMemberChunk, UiProject, UiSearchPage, UiSnapshot};
#[cfg(feature = "hydrate")]
use crate::url::{inspect_url, search_api_url, simple_index_url, simple_project_url, stats_api_url};

/// The dashboard snapshot.
pub async fn load_snapshot() -> UiSnapshot {
    #[cfg(feature = "ssr")]
    {
        crate::ssr::snapshot()
    }
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    {
        send_wrapper::SendWrapper::new(async {
            fetch_json("/+status")
                .await
                .map_or_else(UiSnapshot::default, |value| UiSnapshot::from_status(&value))
        })
        .await
    }
    #[cfg(all(not(feature = "ssr"), not(feature = "hydrate")))]
    {
        UiSnapshot::default()
    }
}

/// The admin status snapshot, including bounded metadata summaries.
pub async fn load_admin_snapshot() -> UiSnapshot {
    #[cfg(feature = "ssr")]
    {
        crate::ssr::admin_snapshot()
    }
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    {
        send_wrapper::SendWrapper::new(async {
            fetch_json("/+status?details=admin")
                .await
                .map_or_else(UiSnapshot::default, |value| UiSnapshot::from_status(&value))
        })
        .await
    }
    #[cfg(all(not(feature = "ssr"), not(feature = "hydrate")))]
    {
        UiSnapshot::default()
    }
}

/// The project names of the index at `route`.
///
/// # Errors
/// Returns a user-visible message when the index cannot be read.
pub async fn load_projects(route: String) -> Result<Vec<String>, String> {
    if route.is_empty() {
        return Ok(Vec::new());
    }
    #[cfg(feature = "ssr")]
    {
        crate::ssr::projects(&route)
    }
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    {
        send_wrapper::SendWrapper::new(async move {
            fetch_json_required(&simple_index_url(&route))
                .await
                .map(|value| crate::model::projects_from_list(&value))
        })
        .await
    }
    #[cfg(all(not(feature = "ssr"), not(feature = "hydrate")))]
    {
        Ok(Vec::new())
    }
}

/// One project's page data: its files, and the parsed core metadata of its newest wheel that
/// advertises a PEP 658 sibling.
///
/// # Errors
/// Returns a user-visible message when the project page or metadata sibling cannot be read.
pub async fn load_project(
    route: String,
    project: String,
) -> Result<Option<(UiProject, Option<CoreMetadataDoc>)>, String> {
    #[cfg(feature = "ssr")]
    {
        crate::ssr::project(&route, &project).await
    }
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    {
        send_wrapper::SendWrapper::new(async move {
            let Some(value) = fetch_json_optional(&simple_project_url(&route, &project)).await? else {
                return Ok(None);
            };
            let ui = UiProject::from_detail(&value);
            let doc = match ui.files.iter().rev().find(|file| file.has_metadata) {
                Some(file) => {
                    let text = fetch_text_required(&format!("{}.metadata", file.url)).await?;
                    Some(velodex_ecosystem_pypi::parse_metadata(&text))
                }
                None => None,
            };
            Ok(Some((ui, doc)))
        })
        .await
    }
    #[cfg(all(not(feature = "ssr"), not(feature = "hydrate")))]
    {
        let _ = (route, project);
        Ok(None)
    }
}

/// Search cached packages.
///
/// # Errors
/// Returns a user-visible message when search parameters are invalid or the index cannot be read.
pub async fn load_search(
    query: String,
    source_type: String,
    page: usize,
    page_size: usize,
) -> Result<UiSearchPage, String> {
    #[cfg(feature = "ssr")]
    {
        crate::ssr::search(&query, &source_type, page, page_size)
    }
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    {
        send_wrapper::SendWrapper::new(async move {
            fetch_json_required(&search_api_url(None, &query, &source_type, page, page_size))
                .await
                .map(|value| UiSearchPage::from_search(&value))
        })
        .await
    }
    #[cfg(all(not(feature = "ssr"), not(feature = "hydrate")))]
    {
        let _ = (query, source_type, page, page_size);
        Ok(UiSearchPage::default())
    }
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
async fn fetch_json(url: &str) -> Option<serde_json::Value> {
    fetch_json_required(url).await.ok()
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
async fn fetch_json_required(url: &str) -> Result<serde_json::Value, String> {
    let Some(value) = fetch_json_optional(url).await? else {
        return Err(format!("404 from {url}: not found"));
    };
    Ok(value)
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
async fn fetch_json_optional(url: &str) -> Result<Option<serde_json::Value>, String> {
    let response = gloo_net::http::Request::get(url)
        .header("accept", "application/vnd.pypi.simple.v1+json, application/json")
        .send()
        .await
        .map_err(|err| format!("request failed for {url}: {err}"))?;
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
        .map_err(|err| format!("invalid JSON from {url}: {err}"))
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
async fn fetch_text_required(url: &str) -> Result<String, String> {
    let response = gloo_net::http::Request::get(url)
        .send()
        .await
        .map_err(|err| format!("request failed for {url}: {err}"))?;
    if !response.ok() {
        return Err(response_error(response, url).await);
    }
    response
        .text()
        .await
        .map_err(|err| format!("response body from {url} could not be read: {err}"))
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
async fn response_error(response: gloo_net::http::Response, url: &str) -> String {
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if text.is_empty() {
        format!("{status} from {url}")
    } else {
        format!("{status} from {url}: {text}")
    }
}

/// Send an authenticated admin request (yank, un-yank, or delete) from the browser. Returns the
/// response body to surface in the UI.
#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
pub async fn admin_request(method: &str, url: &str, token: &str) -> String {
    use base64::Engine as _;
    let credentials = base64::engine::general_purpose::STANDARD.encode(format!("__token__:{token}"));
    let request = match method {
        "PUT" => gloo_net::http::Request::put(url),
        "DELETE" => gloo_net::http::Request::delete(url),
        _ => gloo_net::http::Request::get(url),
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
}

/// The member listing of a cached archive.
///
/// # Errors
/// Returns a user-visible message when the archive cannot be fetched, listed, or decoded.
pub async fn load_members(
    route: String,
    sha256: String,
    filename: String,
    containers: Vec<String>,
) -> Result<Vec<UiMember>, String> {
    #[cfg(feature = "ssr")]
    {
        crate::ssr::members(&route, &sha256, &filename, &containers).await
    }
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    {
        send_wrapper::SendWrapper::new(async move {
            fetch_json_required(&inspect_url(&route, &sha256, &filename, &containers, None, 0))
                .await
                .map(|value| crate::model::members_from_listing(&value))
        })
        .await
    }
    #[cfg(all(not(feature = "ssr"), not(feature = "hydrate")))]
    {
        let _ = (route, sha256, filename, containers);
        Ok(Vec::new())
    }
}

/// One archive member chunk, rendered as text.
///
/// # Errors
/// Returns a user-visible message when the member cannot be previewed as text.
pub async fn load_member_chunk(
    route: String,
    sha256: String,
    filename: String,
    containers: Vec<String>,
    member: String,
    offset: u64,
) -> Result<UiMemberChunk, String> {
    #[cfg(feature = "ssr")]
    {
        crate::ssr::member_chunk(&route, &sha256, &filename, &containers, &member, offset).await
    }
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    {
        send_wrapper::SendWrapper::new(async move {
            fetch_member_chunk(&inspect_url(
                &route,
                &sha256,
                &filename,
                &containers,
                Some(&member),
                offset,
            ))
            .await
        })
        .await
    }
    #[cfg(all(not(feature = "ssr"), not(feature = "hydrate")))]
    {
        let _ = (route, sha256, filename, containers, member, offset);
        Ok(UiMemberChunk::default())
    }
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
async fn fetch_member_chunk(url: &str) -> Result<UiMemberChunk, String> {
    let response = gloo_net::http::Request::get(url)
        .send()
        .await
        .map_err(|err| format!("request failed for {url}: {err}"))?;
    if !response.ok() {
        return Err(response_error(response, url).await);
    }
    let content_type = response.headers().get("content-type").unwrap_or_default();
    if !content_type.starts_with("text/plain") {
        return Err(format!("{url} returned {content_type}; text/plain expected"));
    }
    let size = parse_header_u64(&response, "x-velodex-member-size");
    let offset = parse_header_u64(&response, "x-velodex-member-offset").unwrap_or_default();
    let next_offset = parse_header_u64(&response, "x-velodex-next-offset");
    Ok(UiMemberChunk {
        text: response
            .text()
            .await
            .map_err(|err| format!("response body from {url} could not be read: {err}"))?,
        size,
        offset,
        next_offset,
    })
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
fn parse_header_u64(response: &gloo_net::http::Response, name: &str) -> Option<u64> {
    response.headers().get(name)?.parse().ok()
}

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
            fetch_json(&stats_api_url(index.as_deref(), project.as_deref()))
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
