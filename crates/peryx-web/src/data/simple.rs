#![allow(
    clippy::future_not_send,
    reason = "browser fetch futures are single-threaded by nature; callers wrap them in SendWrapper"
)]

use peryx_core::UiMeta;

use peryx_core::UiProject;

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
            super::fetch_json_required(&crate::url::simple_index_url(&route))
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

/// One project's page data: its files, and the neutral metadata view of its newest artifact that
/// carries a metadata sibling.
///
/// # Errors
/// Returns a user-visible message when the project page or metadata sibling cannot be read.
pub async fn load_project(route: String, project: String) -> Result<Option<(UiProject, UiMeta)>, String> {
    #[cfg(feature = "ssr")]
    {
        crate::ssr::project(&route, &project).await
    }
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    {
        send_wrapper::SendWrapper::new(async move {
            let Some(value) = super::fetch_json_optional(&crate::url::simple_project_url(&route, &project)).await?
            else {
                return Ok(None);
            };
            let ui = peryx_ecosystem_pypi::ui_project_from_detail(&value);
            let meta = match ui.files.iter().rev().find(|file| file.has_metadata) {
                Some(file) => {
                    let text = super::fetch_text_required(&format!("{}.metadata", file.url)).await?;
                    peryx_ecosystem_pypi::ui_meta(&text)
                }
                None => UiMeta::default(),
            };
            Ok(Some((ui, meta)))
        })
        .await
    }
    #[cfg(all(not(feature = "ssr"), not(feature = "hydrate")))]
    {
        let _ = (route, project);
        Ok(None)
    }
}
