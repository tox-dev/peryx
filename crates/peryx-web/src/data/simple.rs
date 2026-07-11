#![allow(
    clippy::future_not_send,
    reason = "browser fetch futures are single-threaded by nature; callers wrap them in SendWrapper"
)]

use peryx_core::UiProjectView;

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
            let value = super::fetch_json_required(&crate::url::ui_projects_url(&route)).await?;
            serde_json::from_value(value).map_err(|err| format!("invalid project list for {route:?}: {err}"))
        })
        .await
    }
    #[cfg(all(not(feature = "ssr"), not(feature = "hydrate")))]
    {
        Ok(Vec::new())
    }
}

/// One project's browse view: a file listing with metadata (a file ecosystem) or a list of references
/// (a registry), chosen by the index's ecosystem driver. `None` when the project is absent.
///
/// # Errors
/// Returns a user-visible message when the project view cannot be read.
pub async fn load_project_view(route: String, project: String) -> Result<Option<UiProjectView>, String> {
    #[cfg(feature = "ssr")]
    {
        crate::ssr::project_view(&route, &project).await
    }
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    {
        send_wrapper::SendWrapper::new(async move {
            let Some(value) = super::fetch_json_optional(&crate::url::ui_project_url(&route, &project)).await? else {
                return Ok(None);
            };
            serde_json::from_value(value)
                .map(Some)
                .map_err(|err| format!("invalid project view for {project:?} on {route:?}: {err}"))
        })
        .await
    }
    #[cfg(all(not(feature = "ssr"), not(feature = "hydrate")))]
    {
        let _ = (route, project);
        Ok(None)
    }
}
