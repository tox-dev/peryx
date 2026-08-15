use std::sync::Arc;

use axum::http::HeaderMap;
use leptos::prelude::*;
use peryx_driver::AppState;

use crate::model::UiLoginState;

#[must_use]
pub async fn login_state() -> UiLoginState {
    let app = expect_context::<Arc<AppState>>();
    let headers = leptos_axum::extract::<HeaderMap>().await.unwrap_or_default();
    let user = match peryx_http::handlers::session_user(&app, &headers) {
        Some(user) => Some(user.name.display().to_owned()),
        None => None,
    };
    let providers = app.serving.oidc_providers().into_iter().map(str::to_owned).collect();
    UiLoginState { user, providers }
}
