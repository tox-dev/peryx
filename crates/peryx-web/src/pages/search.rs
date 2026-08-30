use leptos::either::Either;
use leptos::prelude::*;
use leptos_router::hooks::use_query_map;

use super::{ErrorMessage, reactive_value};
use crate::data::load_search;
use crate::model::{UiSearchPage, UiSearchResult};
use crate::url::{browse_index_url, search_page_url};

#[component]
#[must_use]
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
    let availability = Memo::new(move |_| {
        query_map
            .read()
            .get("availability")
            .filter(|value| value == "local")
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
        move || {
            (
                reactive_value(&query),
                reactive_value(&source_type),
                reactive_value(&availability),
                reactive_value(&page),
                reactive_value(&page_size),
            )
        },
        |(query, source_type, availability, page, page_size)| {
            load_search(query, source_type, availability, page, page_size)
        },
    );
    view! {
        <section class="page search-page">
            <h1>"Search"</h1>
            <SearchForm
                query=reactive_value(&query)
                source_type=reactive_value(&source_type)
                availability=reactive_value(&availability)
                page_size=reactive_value(&page_size)
            />
            <Suspense fallback=|| view! { <p class="dim">"loading"</p> }>
                {move || {
                    let query = reactive_value(&query);
                    let source_type = reactive_value(&source_type);
                    let availability = reactive_value(&availability);
                    Suspend::new(async move {
                        match results.await {
                            Ok(page) => {
                                view! { <SearchResults query source_type availability page_data=page /> }
                                    .into_any()
                            }
                            Err(message) => view! { <ErrorMessage message /> }.into_any(),
                        }
                    })
                }}
            </Suspense>
        </section>
    }
}

#[component]
#[must_use]
fn SearchForm(query: String, source_type: String, availability: String, page_size: usize) -> impl IntoView {
    view! {
        <form class="search-controls" method="get" action="/search">
            <input class="search" type="search" name="q" value=query placeholder="Search indexed entries" />
            <select name="type" aria-label="Source type">
                <option value="all" selected=source_type == "all">"All"</option>
                <option value="uploaded" selected=source_type == "uploaded">"Uploaded"</option>
                <option value="cached" selected=source_type == "cached">"Cached"</option>
                <option value="override" selected=source_type == "override">"Override"</option>
            </select>
            <select name="availability" aria-label="Availability">
                <option value="all" selected=availability == "all">"Any availability"</option>
                <option value="local" selected=availability == "local">"Local only"</option>
            </select>
            <select name="page_size" aria-label="Page size">
                <option value="25" selected=page_size == 25>"25"</option>
                <option value="50" selected=page_size == 50>"50"</option>
                <option value="100" selected=page_size == 100>"100"</option>
            </select>
            <button type="submit">"Search"</button>
        </form>
    }
}

#[component]
#[must_use]
fn SearchResults(query: String, source_type: String, availability: String, page_data: UiSearchPage) -> impl IntoView {
    if page_data.total == 0 {
        let message = if query.trim().is_empty() {
            "Nothing indexed yet. Cached resources appear after their artifacts are requested."
        } else {
            "Nothing matched this search."
        };
        return Either::Left(view! { <p class="dim">{message}</p> });
    }
    let Some((start, end)) = page_data.shown_range() else {
        let last_page = page_data.total.div_ceil(page_data.page_size);
        let href = search_page_url(&query, &source_type, &availability, last_page, page_data.page_size);
        return Either::Right(Either::Left(view! {
            <p class="dim">"This page is past the last result of "{page_data.total}"."</p>
            <nav class="pagination" aria-label="Search pages">
                <a class="page-link" href=href>"Go to last page"</a>
            </nav>
        }));
    };
    let UiSearchPage {
        page,
        page_size,
        total,
        results,
        ..
    } = page_data;
    let previous = (page > 1).then(|| search_page_url(&query, &source_type, &availability, page - 1, page_size));
    let next = (end < total).then(|| search_page_url(&query, &source_type, &availability, page + 1, page_size));
    Either::Right(Either::Right(view! {
        <p class="result-count">"Showing "{start}"-"{end}" of "{total}</p>
        <div class="table-scroll">
            <table class="files search-results">
                <thead>
                    <tr>
                        <th>"Name"</th>
                        <th>"Type"</th>
                        <th>"Normalized"</th>
                        <th>"Source"</th>
                        <th>"Availability"</th>
                        <th>"Index"</th>
                        <th>"Summary"</th>
                    </tr>
                </thead>
                <tbody>
                    {results
                        .into_iter()
                        .map(|result| view! { <SearchResult result /> })
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
            <span>"Page "{page}</span>
            {next
                .map_or_else(
                    || view! { <span class="page-link disabled">"Next"</span> }.into_any(),
                    |href| view! { <a class="page-link" href=href>"Next"</a> }.into_any(),
                )}
        </nav>
    }))
}

#[component]
#[must_use]
fn SearchResult(result: UiSearchResult) -> impl IntoView {
    let href = browse_index_url(&result.route);
    let source_class = format!("badge source-{}", result.source_type);
    let source_title =
        (result.source_type == "override").then_some("Hosted files or overrides affect this upstream entry");
    let (available_class, available_label, available_title) = availability_badge(result.available);
    let source_label = result.source_label();
    view! {
        <tr>
            <td><a href=href>{result.display_label}</a></td>
            <td><span class="badge">{result.type_label}</span></td>
            <td><code>{result.resource_key}</code></td>
            <td><span class=source_class title=source_title>{source_label}</span></td>
            <td><span class=available_class title=available_title>{available_label}</span></td>
            <td><code>{result.index}</code></td>
            <td>{result.summary.unwrap_or_default()}</td>
        </tr>
    }
}

const fn availability_badge(available: bool) -> (&'static str, &'static str, &'static str) {
    if available {
        (
            "badge available-local",
            "local",
            "Bytes are held locally and served without an upstream fetch",
        )
    } else {
        (
            "badge available-remote",
            "remote",
            "No local bytes; a request fetches from upstream if it can",
        )
    }
}

#[cfg(test)]
#[cfg(feature = "ssr")]
#[path = "../../tests/unit/pages/search/tests.rs"]
mod tests;
