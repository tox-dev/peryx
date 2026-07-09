#![allow(
    clippy::future_not_send,
    reason = "browser fetch futures are single-threaded by nature; callers wrap them in SendWrapper"
)]

/// Send an authenticated admin request (yank, un-yank, or delete) from the browser. Returns the
/// response body to surface in the UI.
pub async fn admin_request(method: &str, url: &str, token: &str) -> String {
    use base64::Engine as _;
    let credentials = base64::engine::general_purpose::STANDARD.encode(format!("__token__:{token}"));
    let request = match method {
        "PUT" => gloo_net::http::Request::put(url),
        "DELETE" => gloo_net::http::Request::delete(url),
        _ => gloo_net::http::Request::get(url),
    };
    match request
        .header("authorization", &format!("Basic {credentials}"))
        .send()
        .await
    {
        Ok(response) => {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            format!("{status}: {body}")
        }
        Err(err) => format!("request failed: {err}"),
    }
}
