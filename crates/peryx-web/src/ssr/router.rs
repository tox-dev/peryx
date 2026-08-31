use std::sync::Arc;

use axum::Router;
use axum::extract::FromRef;
use leptos::prelude::*;
use leptos_axum::{AxumRouteListing, LeptosRoutes as _};
use leptos_router::{Method, SsrMode};
use peryx_driver::rate_limit::RouteClass;
use peryx_driver::{AppState, MountedRoutes, RouteDescriptor, RouteMethod, RoutePosture, RouteRateLimit};

use crate::shell;

macro_rules! axum_routes {
    ($(($path:literal, $matcher:expr, $view:ident, $mode:ident, $class:ident)),+ $(,)?) => {
        vec![$(AxumRouteListing::new(
            $path.to_owned(),
            SsrMode::$mode,
            [Method::Get],
            Vec::new(),
        )),+]
    };
}

macro_rules! page_descriptors {
    ($(($path:literal, $matcher:expr, $view:ident, $mode:ident, $class:ident)),+ $(,)?) => {
        vec![$(RouteDescriptor::new(
            RouteMethod::Get,
            $path,
            RoutePosture::Read,
            RouteRateLimit::Class(RouteClass::$class),
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

fn route_descriptors() -> Vec<RouteDescriptor> {
    crate::app_routes!(page_descriptors)
}

/// The rendered pages, paired with their classes so the process-wide middleware can trace, count and
/// throttle them alongside the JSON routes they duplicate.
pub fn ui_pages(app: Arc<AppState>) -> MountedRoutes {
    let options = leptos_options();
    let state = UiState { options, app };
    let listings = route_list();
    let router = Router::new()
        .leptos_routes_with_context(
            &state,
            listings,
            {
                let app = state.app.clone();
                move || provide_context(app.clone())
            },
            {
                let options = state.options.clone();
                move || shell(options.clone())
            },
        )
        .with_state(state);
    MountedRoutes::new(router, route_descriptors())
}

/// Static bytes the browser fetches several of per page load. They stay outside the request
/// middleware so a hydrating page cannot spend a rendering budget on its own assets.
pub fn ui_assets() -> Router {
    let site_root = leptos_options().site_root.to_string();
    Router::new()
        // cargo-leptos and direct server builds emit different Wasm names.
        .route_service(
            "/pkg/peryx_web_bg.wasm",
            tower_http::services::ServeFile::new(format!("{site_root}/pkg/peryx_web.wasm")),
        )
        .nest_service("/pkg", tower_http::services::ServeDir::new(format!("{site_root}/pkg")))
        .route("/favicon.svg", axum::routing::get(favicon))
        .route("/mark.svg", axum::routing::get(mark))
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
