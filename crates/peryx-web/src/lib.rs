//! The peryx web UI: a Leptos application, server-side rendered by the peryx binary and hydrated in
//! the browser for reactivity (live counters, client-side search, and upload management).
//!
//! The `ssr` feature (default) compiles the axum integration; cargo-leptos compiles the same
//! components to wasm with `--no-default-features --features hydrate` for the client bundle. Pages
//! render fully without the bundle, so the server works with no wasm toolchain at all.

// The view! macro produces deeply nested static types; hydration codegen needs headroom.
#![recursion_limit = "256"]
#![allow(
    clippy::must_use_candidate,
    reason = "the #[component] macro consumes attributes, so #[must_use] cannot reach the generated functions"
)]
#![allow(
    clippy::missing_const_for_fn,
    reason = "cfg-split helpers are const only without the hydrate feature; constness cannot vary by cfg"
)]

use leptos::prelude::*;
use leptos_meta::{MetaTags, Style, Title, provide_meta_context};
use leptos_router::components::{Route, Router, Routes};
use leptos_router::path;

use crate::data::load_search;
use crate::model::UiSearchResult;
use crate::url::{browse_project_url, search_page_url};

pub mod data;
pub mod markdown;
pub mod model;
pub mod pages;
#[cfg(feature = "ssr")]
pub mod ssr;
pub mod style;
pub mod url;

use pages::{AdminStatus, Browse, Dashboard, Search, Stats};

/// The HTML document shell used by server rendering: head, hydration scripts, and the app.
#[must_use]
pub fn shell(options: LeptosOptions) -> impl IntoView {
    view! {
        <!DOCTYPE html>
        <html lang="en">
            <head>
                <meta charset="utf-8" />
                <meta name="viewport" content="width=device-width, initial-scale=1" />
                <link rel="icon" type="image/svg+xml" href="/favicon.svg" />
                <script>
                    "(function () { var t = localStorage.getItem('theme'); \
                     if (t === 'light' || t === 'dark') document.documentElement.dataset.theme = t; })();"
                </script>
                <AutoReload options=options.clone() />
                <HydrationScripts options />
                <MetaTags />
            </head>
            <body>
                <App />
            </body>
        </html>
    }
}

/// The application: header, routes, and shared metadata.
#[component]
pub fn App() -> impl IntoView {
    provide_meta_context();
    view! {
        <Style>{style::CSS}</Style>
        <Title text="peryx" />
        <Router>
            <Header />
            <main>
                <Routes fallback=|| view! { <p class="dim">"not found"</p> }>
                    <Route path=path!("/") view=Dashboard />
                    <Route path=path!("/admin/status") view=AdminStatus />
                    <Route path=path!("/browse") view=Browse />
                    <Route path=path!("/search") view=Search />
                    <Route path=path!("/stats") view=Stats />
                </Routes>
            </main>
        </Router>
    }
}

#[component]
fn Header() -> impl IntoView {
    view! {
        <header class="site-header">
            <nav>
                <a class="brand" href="/">
                    <BrandMark />
                    <span>"peryx"</span>
                </a>
                <HeaderSearch />
                <div class="nav-links">
                    <a href="/">"Dashboard"</a>
                    <a href="/search?page_size=25">"Search"</a>
                    <a href="/admin/status">"Status"</a>
                    <a href="https://peryx.readthedocs.io/" rel="external">"Docs"</a>
                    <a href="https://github.com/gaborbernat/peryx" rel="external">"GitHub"</a>
                    <ThemeToggle />
                </div>
            </nav>
        </header>
    }
}

#[component]
fn HeaderSearch() -> impl IntoView {
    let (query, set_query) = signal(String::new());
    let suggestions = Resource::new(
        move || query.get(),
        |query| async move {
            if query.trim().chars().count() < 2 {
                return Ok(Vec::new());
            }
            load_search(query, "all".to_owned(), 1, 25)
                .await
                .map(|page| page.results.into_iter().take(6).collect::<Vec<_>>())
        },
    );
    let (last, set_last) = signal(Vec::<UiSearchResult>::new());
    Effect::new(move |_| {
        if query.get().trim().chars().count() < 2 {
            set_last.set(Vec::new());
            return;
        }
        if let Some(Ok(results)) = suggestions.get() {
            set_last.set(results);
        }
    });
    view! {
        <form class="header-search" method="get" action="/search">
            <input
                type="search"
                name="q"
                autocomplete="off"
                placeholder="Search packages"
                on:input:target=move |event| set_query.set(event.target().value())
            />
            <input type="hidden" name="page_size" value="25" />
            {move || {
                let query = query.get();
                (query.trim().chars().count() >= 2)
                    .then(|| view! {
                        <div class="suggestions">
                            {last
                                .get()
                                .into_iter()
                                .map(|result| {
                                    let href = browse_project_url(&result.route, &result.normalized_name);
                                    view! { <Suggestion result href /> }
                                })
                                .collect_view()}
                            <a class="suggestion all-results" href=search_page_url(&query, "all", 1, 25)>"All results"</a>
                        </div>
                    })
            }}
        </form>
    }
}

#[component]
fn Suggestion(result: UiSearchResult, href: String) -> impl IntoView {
    let source_class = format!("badge source-{}", result.source_type);
    let source_label = result.source_label();
    view! {
        <a class="suggestion" href=href>
            <span>{result.display_name}</span>
            <code>{result.normalized_name}</code>
            <span class=source_class>{source_label}</span>
        </a>
    }
}

/// The falcon mark (a diving peregrine), inline so it needs no asset pipeline.
#[component]
fn BrandMark() -> impl IntoView {
    view! {
        <svg width="24" height="24" viewBox="0 0 100 100" role="img" aria-label="peryx logo">
            <defs>
                <linearGradient id="peryxRust" x1="0" y1="0" x2="1" y2="1">
                    <stop offset="0" stop-color="#F74C00" />
                    <stop offset="1" stop-color="#FFB600" />
                </linearGradient>
            </defs>
            <path fill="url(#peryxRust)" d="M45.2 8.2C40.5 9.5 40.4 10.5 41.3 31.7C41.8 42.9 41.7 43.3 37.4 43.3C34.3 43.3 31.7 44.0 29.7 45.3C27.6 46.6 24.0 46.9 22.7 45.8C20.8 44.4 16.9 36.2 15.3 30.6C14.7 28.7 13.7 25.5 13.0 23.7C12.2 21.8 11.4 19.6 11.1 18.8C9.8 15.2 8.3 14.8 6.8 17.8C6.0 19.3 6.2 20.8 7.7 26.8C8.4 29.9 9.8 36.1 10.7 40.7C12.2 47.7 14.6 57.0 15.6 59.7C15.8 60.2 16.4 62.4 17.0 64.7C19.0 72.4 23.1 79.6 25.5 79.6C26.6 79.6 29.2 78.1 29.2 77.5C29.2 77.1 32.0 75.3 32.9 75.1C35.3 74.6 40.5 79.8 42.6 84.8C43.5 87.1 46.0 91.1 47.2 92.2C47.7 92.7 48.3 92.8 50.0 92.8C51.7 92.8 52.3 92.7 52.8 92.2C54.0 91.1 56.5 87.1 57.4 84.8C59.5 79.8 64.7 74.6 67.1 75.1C68.0 75.3 70.8 77.1 70.8 77.5C70.8 78.1 73.4 79.6 74.5 79.6C76.9 79.6 81.0 72.4 83.0 64.7C83.6 62.4 84.2 60.2 84.4 59.7C85.4 57.0 87.8 47.7 89.3 40.7C90.2 36.1 91.6 29.9 92.3 26.8C93.8 20.8 94.0 19.3 93.2 17.8C91.7 14.8 90.2 15.2 88.9 18.8C88.6 19.6 87.8 21.8 87.0 23.7C86.3 25.5 85.3 28.7 84.7 30.6C83.1 36.2 79.2 44.4 77.3 45.8C76.0 46.9 72.4 46.6 70.3 45.3C68.3 44.0 65.7 43.3 62.6 43.3C58.3 43.3 58.2 42.9 58.7 31.7C59.5 14.2 59.3 11.0 57.4 9.3C55.9 7.8 48.8 7.2 45.2 8.2Z" />
        </svg>
    }
}

/// Light/dark toggle: OS preference by default, explicit choice saved to `localStorage`, matching
/// the documentation site's behavior.
#[component]
fn ThemeToggle() -> impl IntoView {
    view! {
        <button class="theme-toggle" type="button" aria-label="Switch color theme" on:click=|_| toggle_theme()>
            "◐"
        </button>
    }
}

fn toggle_theme() {
    #[cfg(feature = "hydrate")]
    {
        use wasm_bindgen::JsCast as _;
        let Some(window) = web_sys::window() else {
            return;
        };
        let Some(document) = window.document() else {
            return;
        };
        let Some(root) = document.document_element() else {
            return;
        };
        let Ok(root) = root.dyn_into::<web_sys::HtmlElement>() else {
            return;
        };
        let dark = root.dataset().get("theme").map_or_else(
            || {
                window
                    .match_media("(prefers-color-scheme: dark)")
                    .ok()
                    .flatten()
                    .is_some_and(|media| media.matches())
            },
            |theme| theme == "dark",
        );
        let next = if dark { "light" } else { "dark" };
        let _ = root.dataset().set("theme", next);
        if let Ok(Some(storage)) = window.local_storage() {
            let _ = storage.set_item("theme", next);
        }
    }
}

/// The browser entry point: hydrate the server-rendered document.
#[cfg(feature = "hydrate")]
#[wasm_bindgen::prelude::wasm_bindgen]
pub fn hydrate() {
    console_error_panic_hook::set_once();
    leptos::mount::hydrate_body(App);
    // Mark completion so tooling (the Playwright suite) can wait for interactivity.
    if let Some(body) = web_sys::window()
        .and_then(|window| window.document())
        .and_then(|document| document.body())
    {
        let _ = body.dataset().set("hydrated", "true");
    }
}

#[cfg(test)]
mod tests;
