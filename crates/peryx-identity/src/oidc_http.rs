use async_trait::async_trait;

#[async_trait]
pub trait OidcHttpTransport: Send + Sync {
    /// Send a request without following redirects.
    async fn execute(&self, request: reqwest::Request) -> Result<reqwest::Response, reqwest::Error>;
}

#[derive(Debug)]
pub struct ReqwestOidcHttpTransport(pub reqwest::Client);

#[async_trait]
impl OidcHttpTransport for ReqwestOidcHttpTransport {
    async fn execute(&self, request: reqwest::Request) -> Result<reqwest::Response, reqwest::Error> {
        self.0.execute(request).await
    }
}
