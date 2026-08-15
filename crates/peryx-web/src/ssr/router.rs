use std::sync::Arc;

use axum::Router;
use axum::extract::FromRef;
use leptos::prelude::*;
use leptos_axum::{LeptosRoutes as _, generate_route_list};
use peryx_driver::AppState;

use crate::{App, shell};

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

fn route_list() -> Vec<leptos_axum::AxumRouteListing> {
    static ROUTES: std::sync::OnceLock<Vec<leptos_axum::AxumRouteListing>> = std::sync::OnceLock::new();
    ROUTES.get_or_init(|| generate_route_list(App)).clone()
}

pub fn ui_router(app: Arc<AppState>) -> Router {
    let options = leptos_options();
    let site_root = options.site_root.to_string();
    let state = UiState { options, app };
    let routes = route_list();
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
        // cargo-leptos and direct server builds emit different Wasm names.
        .route_service(
            "/pkg/peryx_web_bg.wasm",
            tower_http::services::ServeFile::new(format!("{site_root}/pkg/peryx_web.wasm")),
        )
        .nest_service("/pkg", tower_http::services::ServeDir::new(format!("{site_root}/pkg")))
        .route("/favicon.svg", axum::routing::get(favicon))
        .with_state(state)
}

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

fn leptos_options() -> LeptosOptions {
    LeptosOptions::builder()
        .output_name("peryx_web")
        .site_root("ui")
        .site_pkg_dir("pkg")
        .build()
}

#[cfg(test)]
#[path = "../../tests/unit/ssr/router/tests.rs"]
mod tests;
