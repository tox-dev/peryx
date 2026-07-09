#![allow(
    clippy::future_not_send,
    reason = "browser fetch futures are single-threaded by nature; callers wrap them in SendWrapper"
)]

use crate::model::{UiMember, UiMemberChunk};

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
            super::fetch_json_required(&crate::url::inspect_url(
                &route,
                &sha256,
                &filename,
                &containers,
                None,
                0,
            ))
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
            super::fetch_member_chunk(&crate::url::inspect_url(
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
