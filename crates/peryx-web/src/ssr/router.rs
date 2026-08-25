use std::sync::Arc;

use axum::Router;
use axum::extract::FromRef;
use leptos::prelude::*;
use leptos_axum::{AxumRouteListing, LeptosRoutes as _};
use leptos_router::{Method, SsrMode};
use peryx_driver::AppState;

use crate::shell;

macro_rules! axum_routes {
    ($(($path:literal, $matcher:expr, $view:ident, $mode:ident)),+ $(,)?) => {
        vec![$(AxumRouteListing::new(
            $path.to_owned(),
            SsrMode::$mode,
            [Method::Get],
            Vec::new(),
        )),+]
    };
}

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

fn route_list() -> Vec<AxumRouteListing> {
    crate::app_routes!(axum_routes)
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
        .route("/mark.svg", axum::routing::get(mark))
        .with_state(state)
}

async fn favicon() -> impl axum::response::IntoResponse {
    svg(include_str!("../../../../site/static/icon.svg"))
}

async fn mark() -> impl axum::response::IntoResponse {
    svg(include_str!("../../../../site/static/mark.svg"))
}

fn svg(body: &'static str) -> impl axum::response::IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, "image/svg+xml")], body)
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
