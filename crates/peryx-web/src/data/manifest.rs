#![allow(
    clippy::future_not_send,
    reason = "browser fetch futures are single-threaded by nature; callers wrap them in SendWrapper"
)]

use crate::model::{UiManifest, UiMember, UiMemberChunk};

/// One reference's manifest view under a repository, or `None` when the reference is not served.
///
/// # Errors
/// Returns a user-visible message when the manifest cannot be read.
pub async fn load_manifest(route: String, repo: String, reference: String) -> Result<Option<UiManifest>, String> {
    if route.is_empty() || repo.is_empty() || reference.is_empty() {
        return Ok(None);
    }
    #[cfg(feature = "ssr")]
    {
        crate::ssr::manifest(&route, &repo, &reference).await
    }
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    {
        send_wrapper::SendWrapper::new(async move {
            let Some(value) =
                super::fetch_json_optional(&crate::url::ui_manifest_url(&route, &repo, &reference)).await?
            else {
                return Ok(None);
            };
            serde_json::from_value(value)
                .map(Some)
                .map_err(|err| format!("invalid manifest for {repo:?}:{reference:?} on {route:?}: {err}"))
        })
        .await
    }
    #[cfg(all(not(feature = "ssr"), not(feature = "hydrate")))]
    {
        Ok(None)
    }
}

/// The member listing of one stored layer.
///
/// # Errors
/// Returns a user-visible message when the layer cannot be listed.
pub async fn load_layer_members(route: String, repo: String, digest: String) -> Result<Vec<UiMember>, String> {
    if route.is_empty() || repo.is_empty() || digest.is_empty() {
        return Ok(Vec::new());
    }
    #[cfg(feature = "ssr")]
    {
        crate::ssr::layer_members(&route, &repo, &digest).await
    }
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    {
        send_wrapper::SendWrapper::new(async move {
            let value = super::fetch_json_required(&crate::url::ui_members_url(&route, &repo, &digest)).await?;
            serde_json::from_value(value)
                .map_err(|err| format!("invalid layer members for {digest} on {route:?}: {err}"))
        })
        .await
    }
    #[cfg(all(not(feature = "ssr"), not(feature = "hydrate")))]
    {
        let _ = (route, repo, digest);
        Ok(Vec::new())
    }
}

/// One text member chunk of a stored layer.
///
/// # Errors
/// Returns a user-visible message when the member cannot be previewed as text.
pub async fn load_layer_chunk(
    route: String,
    repo: String,
    digest: String,
    member: String,
    offset: u64,
) -> Result<UiMemberChunk, String> {
    #[cfg(feature = "ssr")]
    {
        crate::ssr::layer_chunk(&route, &repo, &digest, &member, offset).await
    }
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    {
        send_wrapper::SendWrapper::new(async move {
            let value =
                super::fetch_json_required(&crate::url::ui_member_url(&route, &repo, &digest, &member, offset)).await?;
            serde_json::from_value(value)
                .map_err(|err| format!("invalid layer member {member:?} for {digest} on {route:?}: {err}"))
        })
        .await
    }
    #[cfg(all(not(feature = "ssr"), not(feature = "hydrate")))]
    {
        let _ = (route, repo, digest, member, offset);
        Ok(UiMemberChunk::default())
    }
}
