//! HTTP blob transport with bearer authentication, a response byte cap, and whole-blob digest
//! verification. Ranged responses remain unverified. [`CapacityLimited`](crate::blob::CapacityLimited)
//! provides the separate concurrency bound, so this transport does not return
//! [`AtCapacity`](TransportError::AtCapacity).

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use peryx_storage::blob::Digest;
use reqwest::Url;
use reqwest::header::{ACCEPT_ENCODING, RANGE};

use crate::blob::{BlobRequest, BlobTransport, ByteRange};
use crate::client_transport::{
    HttpClientConfigError, HttpClientTransport, ReplicationStatus, classify_status, replication_error,
    require_replication_success,
};
use crate::peer::{TransferLimits, TransportError};

const BLOBS_PATH: &str = "+replication/v1/blobs/sha256/";

#[derive(Debug, thiserror::Error)]
pub enum HttpBlobError {
    #[error("peer replication token must not be empty")]
    EmptyToken,
    #[error("invalid peer URL {0:?}")]
    InvalidBase(String),
}

#[derive(Clone)]
pub struct HttpBlobTransport {
    http: HttpClientTransport,
    blobs_url: Url,
    limits: TransferLimits,
}

impl fmt::Debug for HttpBlobTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpBlobTransport")
            .field("blobs_url", &self.blobs_url)
            .field("limits", &self.limits)
            .field("http", &self.http)
            .finish_non_exhaustive()
    }
}

impl HttpBlobTransport {
    /// # Errors
    /// Returns [`HttpBlobError`] for an empty token or a URL that is not a usable HTTP(S) base.
    ///
    /// # Panics
    /// Panics if the HTTP client cannot be built. The static user agent, duration timeout, and guaranteed
    /// `rustls` provider do not trigger this path.
    pub fn new(
        base: &str,
        token: impl Into<String>,
        limits: TransferLimits,
        timeout: Duration,
    ) -> Result<Self, HttpBlobError> {
        let http = HttpClientTransport::new(base, token, timeout).map_err(map_config_error)?;
        let blobs_url = http.endpoint(BLOBS_PATH);
        Ok(Self {
            http,
            blobs_url,
            limits,
        })
    }
}

fn range_header(range: ByteRange) -> String {
    let last = range.offset.saturating_add(range.length).saturating_sub(1);
    format!("bytes={}-{last}", range.offset)
}

#[async_trait]
impl BlobTransport for HttpBlobTransport {
    async fn fetch_blob(&self, request: BlobRequest) -> Result<Vec<u8>, TransportError> {
        let url = format!("{}{}", self.blobs_url, request.digest.as_str());
        let mut builder = self
            .http
            .get(Url::parse(&url).expect("a digest extends a valid blob URL"));
        builder = builder.header(ACCEPT_ENCODING, "identity");
        if let Some(range) = request.range {
            builder = builder.header(RANGE, range_header(range));
        }
        let response = self.http.send(builder).await.map_err(replication_error)?;
        if classify_status(response.status()) == ReplicationStatus::NotFound {
            return Err(TransportError::BlobNotFound {
                digest: request.digest.as_str().to_owned(),
            });
        }
        require_replication_success(response.status())?;
        let cap = self.limits.max_encoded_bytes.get();
        let body = self.http.read_replication_body(response, cap, true).await?;
        if request.range.is_none() {
            let actual = Digest::of(&body);
            if actual != request.digest {
                return Err(TransportError::DigestMismatch {
                    expected: request.digest.as_str().to_owned(),
                    actual: actual.as_str().to_owned(),
                });
            }
        }
        Ok(body)
    }
}

fn map_config_error(error: HttpClientConfigError) -> HttpBlobError {
    match error {
        HttpClientConfigError::EmptyToken => HttpBlobError::EmptyToken,
        HttpClientConfigError::InvalidBase(base) => HttpBlobError::InvalidBase(base),
    }
}
