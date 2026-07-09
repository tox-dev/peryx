#![allow(
    clippy::future_not_send,
    reason = "browser fetch futures are single-threaded by nature; callers wrap them in SendWrapper"
)]

use crate::model::UiSearchPage;

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
            super::fetch_json_required(&crate::url::search_api_url(None, &query, &source_type, page, page_size))
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
