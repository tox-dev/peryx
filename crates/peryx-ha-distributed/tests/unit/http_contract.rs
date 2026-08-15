use std::fmt::Debug;
use std::future::Future;

use axum::Router;
use axum::response::Response;
use axum::routing::{get, post};

use super::TestServer;

pub fn assert_configuration<Error: Debug>(
    mut construct: impl FnMut(&str, &str) -> Result<(), Error>,
    is_empty_token: impl Fn(&Error) -> bool,
    is_invalid_base: impl Fn(&Error) -> bool,
) {
    let error = construct("http://peer.example/", "").unwrap_err();
    assert!(is_empty_token(&error), "{error:?}");
    for base in ["not a url", "ftp://peer.example/"] {
        let error = construct(base, "secret").unwrap_err();
        assert!(is_invalid_base(&error), "{base}: {error:?}");
    }
}

pub fn assert_redacted(value: &impl Debug, secret: &str, visible: &[&str]) {
    let rendered = format!("{value:?}");
    assert!(visible.iter().all(|value| rendered.contains(value)), "{rendered}");
    assert!(!rendered.contains(secret), "{rendered}");
}

pub fn fixed_get(path: &'static str, response: impl Fn() -> Response + Clone + Send + Sync + 'static) -> Router {
    Router::new().route(
        path,
        get(move || {
            let response = response.clone();
            async move { response() }
        }),
    )
}

pub fn fixed_post(path: &'static str, response: impl Fn() -> Response + Clone + Send + Sync + 'static) -> Router {
    Router::new().route(
        path,
        post(move || {
            let response = response.clone();
            async move { response() }
        }),
    )
}

pub async fn run<Output, Request, RequestFuture>(router: Router, request: Request) -> Output
where
    Request: FnOnce(String) -> RequestFuture,
    RequestFuture: Future<Output = Output>,
{
    let server = TestServer::start(router).await;
    request(server.url.clone()).await
}

pub async fn run_nested<Output, Request, RequestFuture>(router: Router, request: Request) -> Output
where
    Request: FnOnce(String) -> RequestFuture,
    RequestFuture: Future<Output = Output>,
{
    let server = TestServer::start(Router::new().nest("/mirror", router)).await;
    request(format!("{}mirror", server.url)).await
}

pub async fn assert_mapping<Output, Request, RequestFuture>(router: Router, request: Request, expected: Output)
where
    Output: Debug + PartialEq,
    Request: FnOnce(String) -> RequestFuture,
    RequestFuture: Future<Output = Output>,
{
    assert_eq!(run(router, request).await, expected);
}
