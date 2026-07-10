use std::sync::Arc;

use leptos::prelude::*;
use peryx_driver::AppState;

use crate::model::{UiMember, UiMemberChunk, UiOciManifest, members_from_listing};

/// The tags of one OCI repository, read by driving the `/v2/` registry driver in process.
///
/// # Errors
/// Returns a user-visible message when the `/v2/` response cannot be read or parsed.
pub async fn oci_tags(route: &str, repo: &str) -> Result<Vec<String>, String> {
    let app = expect_context::<Arc<AppState>>();
    let value = oci_get(&app, &crate::url::oci_tags_url(route, repo)).await?;
    Ok(value["tags"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tag| tag.as_str().map(str::to_owned))
        .collect())
}

/// One tag's parsed manifest, or `None` when the reference is not served.
///
/// # Errors
/// Returns a user-visible message when the `/v2/` response cannot be read or parsed.
pub async fn oci_manifest(route: &str, repo: &str, reference: &str) -> Result<Option<UiOciManifest>, String> {
    let app = expect_context::<Arc<AppState>>();
    let value = oci_get(&app, &crate::url::oci_manifest_url(route, repo, reference)).await?;
    Ok((!value.is_null()).then(|| UiOciManifest::from_json(&value)))
}

/// The member listing of a stored OCI layer, for server rendering, read by driving the `/v2/` layer
/// browser in process.
///
/// # Errors
/// Returns a user-visible message when the layer response cannot be read or parsed.
pub async fn oci_layer_members(route: &str, repo: &str, digest: &str) -> Result<Vec<UiMember>, String> {
    let app = expect_context::<Arc<AppState>>();
    let value = oci_get(&app, &crate::url::oci_layer_inspect_url(route, repo, digest, None, 0)).await?;
    Ok(members_from_listing(&value))
}

/// One text member chunk of a stored OCI layer, for server rendering.
///
/// # Errors
/// Returns a user-visible message when the layer, member, or its text cannot be read.
pub async fn oci_layer_chunk(
    route: &str,
    repo: &str,
    digest: &str,
    member: &str,
    offset: u64,
) -> Result<UiMemberChunk, String> {
    let app = expect_context::<Arc<AppState>>();
    let uri = crate::url::oci_layer_inspect_url(route, repo, digest, Some(member), offset);
    let request = axum::extract::Request::builder()
        .method("GET")
        .uri(&uri)
        .body(axum::body::Body::empty())
        .map_err(|err| err.to_string())?;
    let response = serve_namespace(&app, request)
        .await
        .ok_or_else(|| format!("layer member {member:?} on index {route:?}: no registry configured"))?;
    if !response.status().is_success() {
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .map_err(|err| err.to_string())?;
        return Err(format!(
            "layer member {member:?} on index {route:?} for {digest}: {status}: {}",
            String::from_utf8_lossy(&bytes)
        ));
    }
    let size = header_u64(response.headers(), "x-peryx-member-size");
    let chunk_offset = header_u64(response.headers(), "x-peryx-member-offset").unwrap_or_default();
    let next_offset = header_u64(response.headers(), "x-peryx-next-offset");
    let bytes = axum::body::to_bytes(response.into_body(), 4 << 20)
        .await
        .map_err(|err| err.to_string())?;
    Ok(UiMemberChunk {
        text: String::from_utf8(bytes.to_vec()).map_err(|err| {
            format!("layer member {member:?} on index {route:?} for {digest} is not valid UTF-8: {err}")
        })?,
        size,
        offset: chunk_offset,
        next_offset,
    })
}

/// Parse a `u64` response header, or `None` when it is absent or unparsable.
fn header_u64(headers: &axum::http::HeaderMap, name: &str) -> Option<u64> {
    headers.get(name)?.to_str().ok()?.parse().ok()
}

/// Serve an in-process request through the namespace driver that owns its path (`OCI`'s `/v2/`), or
/// `None` when no namespace driver is configured.
async fn serve_namespace(app: &Arc<AppState>, request: axum::extract::Request) -> Option<axum::response::Response> {
    let driver = app.namespace_for_path(request.uri().path())?.clone();
    Some(driver.serve(app.clone(), request).await)
}

/// Issue a `/v2/` GET against the in-process registry driver and parse a JSON body.
async fn oci_get(app: &Arc<AppState>, uri: &str) -> Result<serde_json::Value, String> {
    let request = axum::extract::Request::builder()
        .method("GET")
        .uri(uri)
        .body(axum::body::Body::empty())
        .map_err(|err| err.to_string())?;
    let Some(response) = serve_namespace(app, request).await else {
        return Ok(serde_json::Value::Null);
    };
    if !response.status().is_success() {
        return Ok(serde_json::Value::Null);
    }
    let bytes = axum::body::to_bytes(response.into_body(), 4 << 20)
        .await
        .map_err(|err| err.to_string())?;
    serde_json::from_slice(&bytes).map_err(|err| err.to_string())
}
