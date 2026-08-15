mod client_tests;
mod credential_tests;
mod error_tests;
#[cfg(unix)]
mod exec_tests;
mod netrc_tests;
mod property_tests;
mod retry_tests;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::client::UpstreamClient;

pub(super) async fn mount_get(server: &MockServer, request_path: &str, response: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path(request_path))
        .respond_with(response)
        .mount(server)
        .await;
}

pub(super) fn guarded_client(server: &MockServer) -> UpstreamClient {
    UpstreamClient::new(&format!("{}/api/", server.uri())).unwrap()
}
