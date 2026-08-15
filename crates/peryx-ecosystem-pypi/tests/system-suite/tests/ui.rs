use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http_body_util::BodyExt as _;
use peryx_identity::{GrantScope, Role};
use tower::ServiceExt as _;

pub use peryx::{config, server};

mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use peryx::config::{SecretSource, TokenConfig, UpstreamConfig, UpstreamRoutingConfig, UpstreamTlsConfig};
    use peryx_identity::Action;

    pub fn writer_token(secret: SecretSource) -> TokenConfig {
        TokenConfig {
            name: "uploader".to_owned(),
            secret,
            resources: vec!["*".to_owned()],
            actions: BTreeSet::from([Action::Write, Action::Delete]),
            expires_at: None,
        }
    }

    pub fn single_route(url: &str) -> UpstreamRoutingConfig {
        UpstreamRoutingConfig {
            upstreams: vec![UpstreamConfig {
                name: "primary".to_owned(),
                url: url.to_owned(),
                artifact_url: None,
                username: None,
                password: None,
                token: None,
                credential_exec: None,
                credential_refresh: None,
                tls: UpstreamTlsConfig::default(),
            }],
            fallback: true,
            protected: Vec::new(),
            pins: BTreeMap::new(),
        }
    }
}

const ADMIN_PASSWORD: &str = "local password";

async fn seed_administrator(state: &peryx_driver::AppState) -> String {
    let user = state.serving.users.create("Alice").unwrap();
    state
        .serving
        .users
        .set_password(&user.id, ADMIN_PASSWORD)
        .await
        .unwrap();
    state
        .serving
        .authorization
        .grant(&user.id, Role::Administrator, GrantScope::Server)
        .unwrap();
    format!("Basic {}", STANDARD.encode(format!("Alice:{ADMIN_PASSWORD}")))
}

async fn get(router: &axum::Router, uri: &str) -> (StatusCode, String) {
    get_authorized(router, uri, "").await
}

async fn get_authorized(router: &axum::Router, uri: &str, authorization: &str) -> (StatusCode, String) {
    let mut request = Request::builder().uri(uri);
    if !authorization.is_empty() {
        request = request.header(header::AUTHORIZATION, authorization);
    }
    let _render = render_gate().lock().await;
    let response = router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn render_gate() -> &'static tokio::sync::Mutex<()> {
    static GATE: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    GATE.get_or_init(tokio::sync::Mutex::default)
}

#[path = "cases/ui.rs"]
mod cases;
