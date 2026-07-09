#![allow(
    clippy::future_not_send,
    reason = "browser fetch futures are single-threaded by nature; callers wrap them in SendWrapper"
)]

use crate::model::{UiMember, UiMemberChunk, UiOciManifest};

/// The tags of one OCI repository under an index route.
///
/// # Errors
/// Returns a user-visible message when the tag list cannot be read.
pub async fn load_oci_tags(route: String, repo: String) -> Result<Vec<String>, String> {
    if route.is_empty() || repo.is_empty() {
        return Ok(Vec::new());
    }
    #[cfg(feature = "ssr")]
    {
        crate::ssr::oci_tags(&route, &repo).await
    }
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    {
        send_wrapper::SendWrapper::new(async move {
            super::fetch_json_required(&crate::url::oci_tags_url(&route, &repo))
                .await
                .map(|value| {
                    value["tags"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|tag| tag.as_str().map(str::to_owned))
                        .collect()
                })
        })
        .await
    }
    #[cfg(all(not(feature = "ssr"), not(feature = "hydrate")))]
    {
        Ok(Vec::new())
    }
}

/// One tag's parsed manifest under an OCI repository, or `None` when the reference is not served.
///
/// # Errors
/// Returns a user-visible message when the manifest cannot be read.
pub async fn load_oci_manifest(
    route: String,
    repo: String,
    reference: String,
) -> Result<Option<UiOciManifest>, String> {
    if route.is_empty() || repo.is_empty() || reference.is_empty() {
        return Ok(None);
    }
    #[cfg(feature = "ssr")]
    {
        crate::ssr::oci_manifest(&route, &repo, &reference).await
    }
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    {
        send_wrapper::SendWrapper::new(async move {
            Ok(
                super::fetch_json_optional(&crate::url::oci_manifest_url(&route, &repo, &reference))
                    .await?
                    .map(|value| UiOciManifest::from_json(&value)),
            )
        })
        .await
    }
    #[cfg(all(not(feature = "ssr"), not(feature = "hydrate")))]
    {
        Ok(None)
    }
}

/// The repositories an OCI index holds, from the search index scoped to that route.
///
/// # Errors
/// Returns a user-visible message when the repository list cannot be read.
pub async fn load_oci_repositories(route: String) -> Result<Vec<String>, String> {
    if route.is_empty() {
        return Ok(Vec::new());
    }
    #[cfg(feature = "ssr")]
    {
        crate::ssr::repositories(&route)
    }
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    {
        send_wrapper::SendWrapper::new(async move {
            super::fetch_json_required(&crate::url::search_api_url(Some(&route), "", "all", 1, 100))
                .await
                .map(|value| {
                    value["results"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|result| result["display_name"].as_str().map(str::to_owned))
                        .collect()
                })
        })
        .await
    }
    #[cfg(all(not(feature = "ssr"), not(feature = "hydrate")))]
    {
        Ok(Vec::new())
    }
}

/// The member listing of one stored OCI layer.
///
/// # Errors
/// Returns a user-visible message when the layer cannot be listed.
pub async fn load_oci_layer_members(route: String, repo: String, digest: String) -> Result<Vec<UiMember>, String> {
    if route.is_empty() || repo.is_empty() || digest.is_empty() {
        return Ok(Vec::new());
    }
    #[cfg(feature = "ssr")]
    {
        crate::ssr::oci_layer_members(&route, &repo, &digest).await
    }
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    {
        send_wrapper::SendWrapper::new(async move {
            super::fetch_json_required(&crate::url::oci_layer_inspect_url(&route, &repo, &digest, None, 0))
                .await
                .map(|value| crate::model::members_from_listing(&value))
        })
        .await
    }
    #[cfg(all(not(feature = "ssr"), not(feature = "hydrate")))]
    {
        let _ = (route, repo, digest);
        Ok(Vec::new())
    }
}

/// One text member chunk of a stored OCI layer.
///
/// # Errors
/// Returns a user-visible message when the member cannot be previewed as text.
pub async fn load_oci_layer_chunk(
    route: String,
    repo: String,
    digest: String,
    member: String,
    offset: u64,
) -> Result<UiMemberChunk, String> {
    #[cfg(feature = "ssr")]
    {
        crate::ssr::oci_layer_chunk(&route, &repo, &digest, &member, offset).await
    }
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    {
        send_wrapper::SendWrapper::new(async move {
            super::fetch_member_chunk(&crate::url::oci_layer_inspect_url(
                &route,
                &repo,
                &digest,
                Some(&member),
                offset,
            ))
            .await
        })
        .await
    }
    #[cfg(all(not(feature = "ssr"), not(feature = "hydrate")))]
    {
        let _ = (route, repo, digest, member, offset);
        Ok(UiMemberChunk::default())
    }
}
