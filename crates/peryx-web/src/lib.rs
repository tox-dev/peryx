use leptos::prelude::*;
use leptos_meta::{MetaTags, Style, Title, provide_meta_context};
use leptos_router::components::{Route, Router, Routes};
use leptos_router::hooks::use_query_map;
use leptos_router::{SsrMode, StaticSegment};

use crate::data::load_search;
use crate::markdown::EXTERNAL_LINK_REL;
use crate::model::UiSearchResult;
use crate::url::browse_index_url;
use crate::url::search_page_url;

macro_rules! app_routes {
    ($consumer:ident) => {
        $consumer! {
            ("/", StaticSegment("/"), Dashboard, Async),
            ("/admin/status", (StaticSegment("/admin"), StaticSegment("status")), AdminStatus, Async),
            ("/admin/topology", (StaticSegment("/admin"), StaticSegment("topology")), AvailabilityTopology, Async),
            ("/admin/placements", (StaticSegment("/admin"), StaticSegment("placements")), ArtifactPlacements, Async),
            ("/admin/operations", (StaticSegment("/admin"), StaticSegment("operations")), PendingOperations, Async),
            (
                "/admin/policy-decisions",
                (StaticSegment("/admin"), StaticSegment("policy-decisions")),
                PolicyDecisions,
                OutOfOrder
            ),
            ("/admin/trash", (StaticSegment("/admin"), StaticSegment("trash")), Trash, OutOfOrder),
            ("/admin/analytics", (StaticSegment("/admin"), StaticSegment("analytics")), UsageAnalytics, OutOfOrder),
            ("/browse", StaticSegment("/browse"), Browse, Async),
            ("/search", StaticSegment("/search"), Search, Async),
            ("/stats", StaticSegment("/stats"), Stats, Async),
            ("/login", StaticSegment("/login"), Login, Async),
        }
    };
}

macro_rules! route_paths {
    ($(($path:literal, $matcher:expr, $view:ident, $mode:ident)),+ $(,)?) => {
        pub const ROUTE_PATHS: &[&str] = &[$($path),+];
    };
}

app_routes!(route_paths);

#[cfg(feature = "ssr")]
pub(crate) use app_routes;

pub mod data;
pub mod markdown;
pub mod model;
pub mod pages;
#[cfg(feature = "ssr")]
pub mod ssr;
pub mod style;
pub mod url;

use pages::{
    AdminStatus, ArtifactPlacements, AvailabilityTopology, Browse, Dashboard, Login, PendingOperations,
    PolicyDecisions, Search, Stats, Trash, UsageAnalytics,
};

pub use app as App;

macro_rules! app_view {
    ($(($path:literal, $matcher:expr, $view:ident, $mode:ident)),+ $(,)?) => {
        view! {
            <Style>{style::CSS}</Style>
            <Title text="peryx" />
            <Router>
                <Header />
                <main>
                    // In-order rendering prevents Suspense fallbacks from truncating responses under load.
                    <Routes fallback=|| view! { <p class="dim">"not found"</p> }>
                        $(<Route path=$matcher view=$view ssr=SsrMode::$mode />)+
                    </Routes>
                </main>
            </Router>
        }
    };
}

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
                {App()}
            </body>
        </html>
    }
}

#[must_use]
pub fn app() -> impl IntoView {
    provide_meta_context();
    app_routes!(app_view)
}

const DOCS_URL: &str = "https://peryx.readthedocs.io/";
const REPO_URL: &str = "https://github.com/tox-dev/peryx";

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
                    <a href="/admin/topology">"Topology"</a>
                    <a href="/admin/placements">"Placement"</a>
                    <a href="/admin/operations">"Operations"</a>
                    <a href="/admin/policy-decisions">"Policy"</a>
                    <a href="/admin/trash">"Trash"</a>
                    <a href="/admin/analytics">"Usage"</a>
                    <a href="/login">"Login"</a>
                    <a href=DOCS_URL rel=EXTERNAL_LINK_REL>"Docs"</a>
                    <a href=REPO_URL rel=EXTERNAL_LINK_REL>"GitHub"</a>
                    <ThemeToggle />
                </div>
            </nav>
        </header>
    }
}

#[component]
fn HeaderSearch() -> impl IntoView {
    let query_map = use_query_map();
    let (query, set_query) = signal(query_map.read_untracked().get("q").unwrap_or_default());
    let suggestions = Resource::new(
        move || query.get(),
        |query| async move {
            if query.trim().chars().nth(1).is_none() {
                return Ok(Vec::new());
            }
            load_search(query, "all".to_owned(), "all".to_owned(), 1, 25)
                .await
                .map(|page| page.results.into_iter().take(6).collect::<Vec<_>>())
        },
    );
    view! {
        <form class="header-search" method="get" action="/search">
            <HeaderSearchInput value=query.get_untracked() set_query />
            <input type="hidden" name="page_size" value="25" />
            <HeaderSuggestions query suggestions />
        </form>
    }
}

#[component]
fn HeaderSuggestions(
    query: ReadSignal<String>,
    suggestions: Resource<Result<Vec<UiSearchResult>, String>>,
) -> impl IntoView {
    view! {
        <Suspense fallback=|| ()>
            {move || Suspend::new(async move {
                suggestion_panel(&query.get(), suggestions.await.unwrap_or_default())
            })}
        </Suspense>
    }
}

fn suggestion_panel(query: &str, results: Vec<UiSearchResult>) -> impl IntoView + use<> {
    (query.trim().chars().count() >= 2).then(|| {
        view! {
            <div class="suggestions">
                <SuggestionList results />
                <a class="suggestion all-results" href=search_page_url(query, "all", "all", 1, 25)>"All results"</a>
            </div>
        }
    })
}

#[component]
#[cfg(not(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate")))]
fn HeaderSearchInput(value: String, set_query: WriteSignal<String>) -> impl IntoView {
    let _ = set_query;
    view! {
        <input type="search" name="q" autocomplete="off" placeholder="Search indexes" value=value />
    }
}

#[component]
#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn HeaderSearchInput(value: String, set_query: WriteSignal<String>) -> impl IntoView {
    view! {
        <input
            type="search"
            name="q"
            autocomplete="off"
            placeholder="Search indexes"
            value=value
            on:input:target=move |event| set_query.set(event.target().value())
        />
    }
}

#[component]
fn SuggestionList(results: Vec<UiSearchResult>) -> impl IntoView {
    results
        .into_iter()
        .map(|result| {
            let href = browse_index_url(&result.route);
            view! { <Suggestion result href /> }
        })
        .collect_view()
}

#[component]
fn Suggestion(result: UiSearchResult, href: String) -> impl IntoView {
    let source_class = format!("badge source-{}", result.source_type);
    let source_label = result.source_label();
    view! {
        <a class="suggestion" href=href>
            <span>{result.display_label}</span>
            <code>{result.resource_key}</code>
            <span class=source_class>{source_label}</span>
        </a>
    }
}

#[component]
fn BrandMark() -> impl IntoView {
    view! { <img src="/mark.svg" width="24" height="24" alt="peryx logo" /> }
}

#[component]
fn ThemeToggle() -> impl IntoView {
    view! {
        <button
            class="theme-toggle"
            type="button"
            aria-label="Switch color theme"
            onclick="var r=document.documentElement,d=r.dataset.theme==='dark',n=d?'light':'dark';r.dataset.theme=n;localStorage.setItem('theme',n)"
        >
            "◐"
        </button>
    }
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate", target_arch = "wasm32"))]
#[wasm_bindgen::prelude::wasm_bindgen]
pub fn hydrate() {
    console_error_panic_hook::set_once();
    leptos::mount::hydrate_body(App);
    // Playwright waits for this marker before testing client-side behavior.
    if let Some(body) = web_sys::window()
        .and_then(|window| window.document())
        .and_then(|document| document.body())
    {
        let _ = body.dataset().set("hydrated", "true");
    }
}

#[cfg(all(target_arch = "wasm32", feature = "wasm-coverage"))]
#[wasm_bindgen::prelude::wasm_bindgen]
#[expect(unsafe_code, reason = "Wasm exports serialize minicov's exclusive capture")]
pub fn capture_coverage() -> Vec<u8> {
    let mut profile = Vec::new();
    unsafe {
        minicov::capture_coverage(&mut profile).expect("coverage profile must serialize");
    }
    profile
}

#[cfg(test)]
#[path = "../tests/unit/tests.rs"]
mod tests;

#[cfg(all(test, feature = "ssr"))]
#[path = "../tests/unit/page_contract_tests.rs"]
mod page_contract_tests;

#[cfg(test)]
#[path = "../tests/unit/ssr_contract.rs"]
mod ssr_contract;
