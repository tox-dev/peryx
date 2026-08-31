use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http_body_util::BodyExt as _;
use tower::ServiceExt as _;

pub use peryx::{config, server};

mod tests {
    use std::collections::BTreeSet;

    use peryx::config::{SecretSource, TokenConfig};
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
}

async fn get_authorized(router: &axum::Router, uri: &str, authorization: &str) -> (StatusCode, String) {
    let mut request = Request::builder().uri(uri);
    if !authorization.is_empty() {
        request = request.header(header::AUTHORIZATION, authorization);
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn reader() -> String {
    format!("Basic {}", STANDARD.encode("__token__:read-secret"))
}

#[path = "cases/access.rs"]
mod cases;
