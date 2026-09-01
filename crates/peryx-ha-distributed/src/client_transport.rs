use std::fmt;
use std::time::Duration;

use reqwest::{Client, ClientBuilder, RequestBuilder, StatusCode, Url};

use crate::peer::TransportError;

const USER_AGENT: &str = concat!("peryx-ha-distributed/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, PartialEq, Eq)]
pub enum HttpClientConfigError {
    EmptyToken,
    InvalidBase(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpClientError {
    Timeout,
    Disconnected,
    BodyTooLarge { limit: u64, actual: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicationStatus {
    Success,
    Unauthenticated,
    NotFound,
    ServerError(u16),
    BadStatus(u16),
}

#[derive(Clone)]
pub struct HttpClientTransport {
    client: Client,
    base: Url,
    token: String,
}

impl fmt::Debug for HttpClientTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpClientTransport")
            .field("base", &self.base)
            .field("token", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl HttpClientTransport {
    /// Bounds every request end to end: connection, response and body transfer share `timeout`.
    pub(super) fn new(base: &str, token: impl Into<String>, timeout: Duration) -> Result<Self, HttpClientConfigError> {
        Self::build(base, token, Client::builder().timeout(timeout))
    }

    /// Bounds only connection establishment, leaving each request to carry the deadline its caller was
    /// granted. Used where a single client serves calls with unrelated budgets.
    pub(super) fn with_connect_timeout(
        base: &str,
        token: impl Into<String>,
        connect: Duration,
    ) -> Result<Self, HttpClientConfigError> {
        Self::build(base, token, Client::builder().connect_timeout(connect))
    }

    fn build(base: &str, token: impl Into<String>, client: ClientBuilder) -> Result<Self, HttpClientConfigError> {
        let token = token.into();
        if token.is_empty() {
            return Err(HttpClientConfigError::EmptyToken);
        }
        let Ok(mut base) = Url::parse(base) else {
            return Err(HttpClientConfigError::InvalidBase(base.to_owned()));
        };
        if !matches!(base.scheme(), "http" | "https") || base.cannot_be_a_base() {
            return Err(HttpClientConfigError::InvalidBase(base.to_string()));
        }
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        base.set_query(None);
        base.set_fragment(None);
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        Ok(Self {
            client: client
                .user_agent(USER_AGENT)
                .build()
                .expect("a static user agent and duration timeout build a reqwest client"),
            base,
            token,
        })
    }

    pub(super) fn endpoint(&self, path: &str) -> Url {
        let mut url = self.base.clone();
        url.set_path(&format!("{}{path}", self.base.path()));
        url
    }

    pub(super) fn get(&self, url: Url) -> RequestBuilder {
        self.client.get(url).bearer_auth(&self.token)
    }

    pub(super) fn head(&self, url: Url) -> RequestBuilder {
        self.client.head(url).bearer_auth(&self.token)
    }

    pub(super) fn post(&self, url: Url) -> RequestBuilder {
        self.client.post(url).bearer_auth(&self.token)
    }

    pub(super) async fn send(&self, request: RequestBuilder) -> Result<reqwest::Response, HttpClientError> {
        request.send().await.map_err(|error| classify_loss(&error))
    }

    pub(super) async fn read_bounded(
        &self,
        mut response: reqwest::Response,
        limit: u64,
        reject_advertised_length: bool,
    ) -> Result<Vec<u8>, HttpClientError> {
        if reject_advertised_length
            && let Some(actual) = response.content_length()
            && actual > limit
        {
            return Err(HttpClientError::BodyTooLarge { limit, actual });
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| classify_loss(&error))? {
            let actual = body.len() as u64 + chunk.len() as u64;
            if actual > limit {
                return Err(HttpClientError::BodyTooLarge { limit, actual });
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }

    pub(super) async fn read_replication_body(
        &self,
        response: reqwest::Response,
        limit: u64,
        reject_advertised_length: bool,
    ) -> Result<Vec<u8>, TransportError> {
        self.read_bounded(response, limit, reject_advertised_length)
            .await
            .map_err(replication_error)
    }

    pub(super) async fn read_small_body(
        &self,
        response: reqwest::Response,
        limit: u64,
    ) -> Result<Vec<u8>, TransportError> {
        self.read_bounded(response, limit, false)
            .await
            .map_err(|error| match error {
                HttpClientError::BodyTooLarge { .. } => TransportError::Malformed,
                other => replication_error(other),
            })
    }
}

pub const fn classify_status(status: StatusCode) -> ReplicationStatus {
    let status = status.as_u16();
    match status {
        200..=299 => ReplicationStatus::Success,
        401 => ReplicationStatus::Unauthenticated,
        404 => ReplicationStatus::NotFound,
        500 | 502..=599 => ReplicationStatus::ServerError(status),
        _ => ReplicationStatus::BadStatus(status),
    }
}

pub const fn require_replication_success(status: StatusCode) -> Result<(), TransportError> {
    match classify_status(status) {
        ReplicationStatus::Success => Ok(()),
        ReplicationStatus::Unauthenticated => Err(TransportError::Unauthenticated),
        ReplicationStatus::ServerError(status) => Err(TransportError::ServerError { status }),
        ReplicationStatus::NotFound | ReplicationStatus::BadStatus(_) => Err(TransportError::BadStatus {
            status: status.as_u16(),
        }),
    }
}

pub const fn replication_error(error: HttpClientError) -> TransportError {
    match error {
        HttpClientError::Timeout => TransportError::Timeout,
        HttpClientError::Disconnected => TransportError::Disconnected,
        HttpClientError::BodyTooLarge { limit, actual } => TransportError::FrameTooLarge { limit, actual },
    }
}

fn classify_loss(error: &reqwest::Error) -> HttpClientError {
    if error.is_timeout() {
        HttpClientError::Timeout
    } else {
        HttpClientError::Disconnected
    }
}
