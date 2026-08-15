use crate::model::UiSearchPage;

/// Search indexed entries.
///
/// # Errors
/// Returns a user-visible message when search parameters are invalid or the index cannot be read.
pub async fn load_search(
    query: String,
    source_type: String,
    availability: String,
    page: usize,
    page_size: usize,
) -> Result<UiSearchPage, String> {
    #[cfg(feature = "ssr")]
    {
        crate::ssr::search(&query, &source_type, &availability, page, page_size).await
    }
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    {
        send_wrapper::SendWrapper::new(async move {
            let response = gloo_net::http::Request::get(&crate::url::search_api_url(
                &query,
                &source_type,
                &availability,
                page,
                page_size,
            ))
            .header("accept", "application/json")
            .send()
            .await
            .map_err(|_| "Search could not be reached.".to_owned())?;
            match response.status() {
                200 => response
                    .json()
                    .await
                    .map_err(|_| "Search returned invalid data.".to_owned()),
                400 => Err("The search request was invalid.".to_owned()),
                401 | 403 => Err("You do not have access to search this index.".to_owned()),
                _ => Err("Search is unavailable.".to_owned()),
            }
        })
        .await
    }
    #[cfg(all(not(feature = "ssr"), not(feature = "hydrate")))]
    {
        let _ = (query, source_type, availability, page, page_size);
        Ok(UiSearchPage::default())
    }
}
