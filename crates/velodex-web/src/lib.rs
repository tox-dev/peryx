//! The velodex web UI: a Leptos application, server-side rendered by the velodex binary and hydrated in
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

pub mod data;
pub mod markdown;
pub mod model;
pub mod pages;
#[cfg(feature = "ssr")]
pub mod ssr;
pub mod style;
pub mod url;

use pages::{Browse, Dashboard, Stats};

/// The HTML document shell used by server rendering: head, hydration scripts, and the app.
#[must_use]
pub fn shell(options: LeptosOptions) -> impl IntoView {
    view! {
        <!DOCTYPE html>
        <html lang="en">
            <head>
                <meta charset="utf-8" />
                <meta name="viewport" content="width=device-width, initial-scale=1" />
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
        <Title text="velodex" />
        <Router>
            <Header />
            <main>
                <Routes fallback=|| view! { <p class="dim">"not found"</p> }>
                    <Route path=path!("/") view=Dashboard />
                    <Route path=path!("/browse") view=Browse />
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
                    <span>"velodex"</span>
                </a>
                <div class="nav-links">
                    <a href="/">"Dashboard"</a>
                    <a href="https://velodex.readthedocs.io/" rel="external">"Docs"</a>
                    <a href="https://github.com/tox-dev/velodex" rel="external">"GitHub"</a>
                    <ThemeToggle />
                </div>
            </nav>
        </header>
    }
}

/// The overlay-stack logo mark, inline so it needs no asset pipeline.
#[component]
fn BrandMark() -> impl IntoView {
    view! {
        <svg width="30" height="20" viewBox="0 0 180 120" role="img" aria-label="velodex logo">
            <defs>
                <linearGradient id="velodexRust" x1="0" y1="0" x2="1" y2="1">
                    <stop offset="0" stop-color="#F74C00" />
                    <stop offset="1" stop-color="#FFB600" />
                </linearGradient>
            </defs>
            <rect x="6" y="60" width="132" height="50" rx="10" fill="#33383E" />
            <rect x="24" y="38" width="132" height="50" rx="10" fill="#4B5058" />
            <rect x="42" y="16" width="132" height="50" rx="10" fill="url(#velodexRust)" />
            <circle cx="108" cy="41" r="10" fill="#1E2226" />
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
