use std::sync::Arc;

use axum::Router;
use axum::extract::FromRef;
use leptos::prelude::*;
use leptos_axum::{LeptosRoutes as _, generate_route_list};
use peryx_http::AppState;

use crate::{App, shell};

/// The router state: leptos options plus the peryx application state.
#[derive(Clone)]
pub struct UiState {
    pub options: LeptosOptions,
    pub app: Arc<AppState>,
}

impl FromRef<UiState> for LeptosOptions {
    fn from_ref(state: &UiState) -> Self {
        state.options.clone()
    }
}

/// Build the UI router.
///
/// The leptos routes (server-rendered, hydration-ready) plus the `/pkg` asset directory holding
/// the wasm bundle. Without the bundle on disk the pages still render; they are just not
/// interactive.
pub fn ui_router(app: Arc<AppState>) -> Router {
    let options = leptos_options();
    let site_root = options.site_root.to_string();
    let state = UiState { options, app };
    let routes = generate_route_list(App);
    Router::new()
        .leptos_routes_with_context(
            &state,
            routes,
            {
                let app = state.app.clone();
                move || provide_context(app.clone())
            },
            {
                let options = state.options.clone();
                move || shell(options.clone())
            },
        )
        // leptos appends `_bg` to the wasm name when the server was not compiled by cargo-leptos
        // (a compile-time env probe), while cargo-leptos writes the file without it; alias the two.
        .route_service(
            "/pkg/peryx_web_bg.wasm",
            tower_http::services::ServeFile::new(format!("{site_root}/pkg/peryx_web.wasm")),
        )
        .nest_service("/pkg", tower_http::services::ServeDir::new(format!("{site_root}/pkg")))
        .route("/favicon.svg", axum::routing::get(favicon))
        .with_state(state)
}

/// The browser-tab icon: the peryx layered-stack mark (no wordmark) on the app's dark tile, with a
/// green node. The documentation site uses the same mark with a blue node (`site/static/icon.svg`),
/// so a tab pinned to a running instance is distinguishable at a glance from a docs tab.
const FAVICON: &str = concat!(
    r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 512 512" role="img" aria-label="peryx">"#,
    r#"<defs><linearGradient id="r" x1="0" y1="0" x2="1" y2="1">"#,
    r##"<stop offset="0" stop-color="#F74C00"/><stop offset="1" stop-color="#FFB600"/></linearGradient></defs>"##,
    r##"<rect width="512" height="512" rx="116" fill="#1E2226"/>"##,
    r#"<g transform="translate(96,132)">"#,
    r##"<rect x="0" y="176" width="300" height="116" rx="28" fill="#4B5058"/>"##,
    r##"<rect x="46" y="104" width="300" height="116" rx="28" fill="#6A7079"/>"##,
    r##"<rect x="92" y="32" width="300" height="116" rx="28" fill="url(#r)"/>"##,
    r##"<circle cx="300" cy="90" r="30" fill="#22C55E"/></g></svg>"##,
);

async fn favicon() -> impl axum::response::IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, "image/svg+xml")], FAVICON)
}

/// The leptos configuration: asset names must match what cargo-leptos produces (`Cargo.toml`
/// workspace metadata), and the site root is where its output lands at runtime.
fn leptos_options() -> LeptosOptions {
    LeptosOptions::builder()
        .output_name("peryx_web")
        .site_root("ui")
        .site_pkg_dir("pkg")
        .build()
}
