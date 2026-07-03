//! Data loading for the UI, compiled per side: the server reads `AppState` directly, the hydrated
//! browser fetches velodex's own JSON API. Both produce the same view models.
#![allow(
    clippy::future_not_send,
    reason = "browser fetch futures are single-threaded by nature; callers wrap them in SendWrapper"
)]

use velodex_core::pypi::CoreMetadataDoc;

use crate::model::{UiMember, UiMemberChunk, UiProject, UiSnapshot};
#[cfg(feature = "hydrate")]
use crate::url::{encode_component, encode_path};

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

/// The project names of the index at `route`.
pub async fn load_projects(route: String) -> Vec<String> {
    #[cfg(feature = "ssr")]
    {
        crate::ssr::projects(&route)
    }
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    {
        send_wrapper::SendWrapper::new(async move {
            fetch_json(&format!("/{route}/simple/"))
                .await
                .map_or_else(Vec::new, |value| crate::model::projects_from_list(&value))
        })
        .await
    }
    #[cfg(all(not(feature = "ssr"), not(feature = "hydrate")))]
    {
        let _ = route;
        Vec::new()
    }
}

/// One project's page data: its files, and the parsed core metadata of its newest wheel that
/// advertises a PEP 658 sibling.
pub async fn load_project(route: String, project: String) -> Option<(UiProject, Option<CoreMetadataDoc>)> {
    #[cfg(feature = "ssr")]
    {
        crate::ssr::project(&route, &project).await
    }
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    {
        send_wrapper::SendWrapper::new(async move {
            let value = fetch_json(&format!("/{route}/simple/{project}/")).await?;
            let ui = UiProject::from_detail(&value);
            let doc = match ui.files.iter().rev().find(|file| file.has_metadata) {
                Some(file) => fetch_text(&format!("{}.metadata", file.url))
                    .await
                    .map(|text| velodex_core::pypi::parse_metadata(&text)),
                None => None,
            };
            Some((ui, doc))
        })
        .await
    }
    #[cfg(all(not(feature = "ssr"), not(feature = "hydrate")))]
    {
        let _ = (route, project);
        None
    }
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
async fn fetch_json(url: &str) -> Option<serde_json::Value> {
    let response = gloo_net::http::Request::get(url)
        .header("accept", "application/vnd.pypi.simple.v1+json, application/json")
        .send()
        .await
        .ok()?;
    if !response.ok() {
        return None;
    }
    response.json().await.ok()
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
async fn fetch_text(url: &str) -> Option<String> {
    let response = gloo_net::http::Request::get(url).send().await.ok()?;
    if !response.ok() {
        return None;
    }
    response.text().await.ok()
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
pub async fn load_members(route: String, sha256: String, filename: String, containers: Vec<String>) -> Vec<UiMember> {
    #[cfg(feature = "ssr")]
    {
        crate::ssr::members(&route, &sha256, &filename, &containers).await
    }
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    {
        send_wrapper::SendWrapper::new(async move {
            fetch_json(&inspect_url(&route, &sha256, &filename, &containers, None, 0))
                .await
                .map_or_else(Vec::new, |value| crate::model::members_from_listing(&value))
        })
        .await
    }
    #[cfg(all(not(feature = "ssr"), not(feature = "hydrate")))]
    {
        let _ = (route, sha256, filename, containers);
        Vec::new()
    }
}

/// One archive member chunk, rendered as text.
pub async fn load_member_chunk(
    route: String,
    sha256: String,
    filename: String,
    containers: Vec<String>,
    member: String,
    offset: u64,
) -> UiMemberChunk {
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
            .unwrap_or_else(|| UiMemberChunk {
                text: "(binary or unavailable)".to_owned(),
                ..UiMemberChunk::default()
            })
        })
        .await
    }
    #[cfg(all(not(feature = "ssr"), not(feature = "hydrate")))]
    {
        let _ = (route, sha256, filename, containers, member, offset);
        UiMemberChunk::default()
    }
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
fn inspect_url(
    route: &str,
    sha256: &str,
    filename: &str,
    containers: &[String],
    member: Option<&str>,
    offset: u64,
) -> String {
    let mut url = format!(
        "/{}/inspect/{}/{}",
        encode_path(route),
        encode_component(sha256),
        encode_component(filename)
    );
    let mut separator = "?";
    for container in containers {
        url.push_str(separator);
        url.push_str("container=");
        url.push_str(&encode_component(container));
        separator = "&";
    }
    if let Some(member) = member {
        url.push_str(separator);
        url.push_str("member=");
        url.push_str(&encode_component(member));
        url.push('&');
        url.push_str("offset=");
        url.push_str(&offset.to_string());
    }
    url
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
async fn fetch_member_chunk(url: &str) -> Option<UiMemberChunk> {
    let response = gloo_net::http::Request::get(url).send().await.ok()?;
    if !response.ok() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Some(UiMemberChunk {
            text: format!("{status}: {text}"),
            ..UiMemberChunk::default()
        });
    }
    let content_type = response.headers().get("content-type").unwrap_or_default();
    if !content_type.starts_with("text/plain") {
        return Some(UiMemberChunk {
            text: "(binary or unavailable)".to_owned(),
            ..UiMemberChunk::default()
        });
    }
    let size = parse_header_u64(&response, "x-velodex-member-size");
    let offset = parse_header_u64(&response, "x-velodex-member-offset").unwrap_or_default();
    let next_offset = parse_header_u64(&response, "x-velodex-next-offset");
    Some(UiMemberChunk {
        text: response.text().await.ok()?,
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
            let mut url = "/+stats".to_owned();
            if let Some(route) = &index {
                url.push_str(&format!("?index={route}"));
                if let Some(name) = &project {
                    url.push_str(&format!("&project={name}"));
                }
            }
            fetch_json(&url).await.map_or_else(Default::default, |value| {
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
