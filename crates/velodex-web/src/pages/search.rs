#![allow(
    clippy::must_use_candidate,
    reason = "the #[component] macro consumes attributes, so #[must_use] cannot reach the generated functions"
)]
#![allow(
    clippy::missing_const_for_fn,
    reason = "cfg-split helpers are const only without the hydrate feature; constness cannot vary by cfg"
)]

use leptos::prelude::*;
use leptos_router::hooks::use_query_map;

use super::ErrorMessage;
use crate::data::load_search;
use crate::model::{UiSearchPage, source_label};
use crate::url::{browse_project_url, search_page_url};

#[component]
pub fn Search() -> impl IntoView {
    let query_map = use_query_map();
    let query = Memo::new(move |_| query_map.read().get("q").unwrap_or_default());
    let source_type = Memo::new(move |_| {
        query_map
            .read()
            .get("type")
            .filter(|value| matches!(value.as_str(), "uploaded" | "cached" | "override"))
            .unwrap_or_else(|| "all".to_owned())
    });
    let page = Memo::new(move |_| {
        query_map
            .read()
            .get("page")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(1)
            .max(1)
    });
    let page_size = Memo::new(move |_| {
        let size = query_map
            .read()
            .get("page_size")
            .and_then(|value| value.parse::<usize>().ok());
        size.filter(|size| matches!(size, 25 | 50 | 100)).unwrap_or(25)
    });
    let results = Resource::new(
        move || (query.get(), source_type.get(), page.get(), page_size.get()),
        |(query, source_type, page, page_size)| load_search(query, source_type, page, page_size),
    );
    view! {
        <section class="page search-page">
            <h1>"Search"</h1>
            <SearchForm query=query.get() source_type=source_type.get() page_size=page_size.get() />
            <Suspense fallback=|| view! { <p class="dim">"loading"</p> }>
                {move || {
                    let query = query.get();
                    let source_type = source_type.get();
                    Suspend::new(async move {
                        match results.await {
                            Ok(page) => view! { <SearchResults query source_type page_data=page /> }.into_any(),
                            Err(message) => view! { <ErrorMessage message /> }.into_any(),
                        }
                    })
                }}
            </Suspense>
        </section>
    }
}

#[component]
fn SearchForm(query: String, source_type: String, page_size: usize) -> impl IntoView {
    view! {
        <form class="search-controls" method="get" action="/search">
            <input class="search" type="search" name="q" value=query placeholder="Search packages and images" />
            <select name="type" aria-label="Source type">
                <option value="all" selected=source_type == "all">"All"</option>
                <option value="uploaded" selected=source_type == "uploaded">"Uploaded"</option>
                <option value="cached" selected=source_type == "cached">"Cached"</option>
                <option value="override" selected=source_type == "override">"Override"</option>
            </select>
            <select
                name="page_size"
                aria-label="Page size"
                on:change:target=move |event| store_search_page_size(&event.target().value())
            >
                <option value="25" selected=page_size == 25>"25"</option>
                <option value="50" selected=page_size == 50>"50"</option>
                <option value="100" selected=page_size == 100>"100"</option>
            </select>
            <button type="submit">"Search"</button>
        </form>
    }
}

fn store_search_page_size(value: &str) {
    #[cfg(feature = "hydrate")]
    {
        if let Some(window) = web_sys::window()
            && let Ok(Some(storage)) = window.local_storage()
        {
            let _ = storage.set_item("velodex.search.page_size", value);
        }
    }
    #[cfg(not(feature = "hydrate"))]
    {
        let _ = value;
    }
}

#[component]
fn SearchResults(query: String, source_type: String, page_data: UiSearchPage) -> impl IntoView {
    if page_data.total == 0 {
        let message = if query.trim().is_empty() {
            "Nothing indexed yet. Cached items appear after their pages or tags are fetched."
        } else {
            "Nothing matched this search."
        };
        return view! { <p class="dim">{message}</p> }.into_any();
    }
    let start = page_data.page.saturating_sub(1).saturating_mul(page_data.page_size) + 1;
    let end = page_data.total.min(start + page_data.results.len().saturating_sub(1));
    let previous =
        (page_data.page > 1).then(|| search_page_url(&query, &source_type, page_data.page - 1, page_data.page_size));
    let next =
        (end < page_data.total).then(|| search_page_url(&query, &source_type, page_data.page + 1, page_data.page_size));
    view! {
        <p class="result-count">"Showing "{start}"-"{end}" of "{page_data.total}</p>
        <div class="table-scroll">
            <table class="files search-results">
                <thead>
                    <tr>
                        <th>"Name"</th>
                        <th>"Type"</th>
                        <th>"Normalized"</th>
                        <th>"Source"</th>
                        <th>"Index"</th>
                        <th>"Summary"</th>
                    </tr>
                </thead>
                <tbody>
                    {page_data
                        .results
                        .into_iter()
                        .map(|result| {
                            let href = browse_project_url(&result.route, &result.normalized_name);
                            let source_class = format!("badge source-{}", result.source_type);
                            let source_title = (result.source_type == "override")
                                .then_some("Hosted files or hosted overrides affect this upstream package");
                            view! {
                                <tr>
                                    <td><a href=href>{result.display_name}</a></td>
                                    <td><span class="badge">{result.type_label}</span></td>
                                    <td><code>{result.normalized_name}</code></td>
                                    <td><span class=source_class title=source_title>{source_label(&result.source_type)}</span></td>
                                    <td><code>{result.index}</code></td>
                                    <td>{result.summary.unwrap_or_default()}</td>
                                </tr>
                            }
                        })
                        .collect_view()}
                </tbody>
            </table>
        </div>
        <nav class="pagination" aria-label="Search pages">
            {previous
                .map_or_else(
                    || view! { <span class="page-link disabled">"Previous"</span> }.into_any(),
                    |href| view! { <a class="page-link" href=href>"Previous"</a> }.into_any(),
                )}
            <span>"Page "{page_data.page}</span>
            {next
                .map_or_else(
                    || view! { <span class="page-link disabled">"Next"</span> }.into_any(),
                    |href| view! { <a class="page-link" href=href>"Next"</a> }.into_any(),
                )}
        </nav>
    }
    .into_any()
}
