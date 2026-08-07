//! A production HTTP [`BlobTransport`]: the concrete client a replica drives against a peer's
//! blob-serving endpoint, in contrast to the in-process [`LoopbackBlobSource`] the tests use.
//!
//! It issues one `GET` per fetch — a whole blob, or a `Range` of it — streams the reply under the
//! transfer byte cap so a fast or hostile peer cannot force an unbounded read, and, for a whole-blob
//! fetch, hashes the streamed bytes and rejects a mismatch with [`TransportError::DigestMismatch`]. A
//! ranged fetch returns its bytes unverified, per the [module integrity contract](crate::blob): the
//! caller reassembles every range and digest-verifies the whole blob at commit. HTTP failures map onto
//! the shared [`TransportError`] taxonomy, so a scheduler acts on a blob loss the same way it acts on a
//! metadata one. The concurrent-stream bound is the [`CapacityLimited`](crate::blob::CapacityLimited)
//! decorator's job, so this transport never raises [`AtCapacity`](TransportError::AtCapacity) itself.
//!
//! [`LoopbackBlobSource`]: crate::blob::LoopbackBlobSource
//! [`BlobTransport`]: crate::blob::BlobTransport

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use peryx_storage::blob::Digest;
use reqwest::header::{ACCEPT_ENCODING, RANGE};
use reqwest::{Client, StatusCode, Url};

use crate::blob::{BlobRequest, BlobTransport, ByteRange};
use crate::peer::{TransferLimits, TransportError};

const BLOBS_PATH: &str = "+replication/v1/blobs/sha256/";
const USER_AGENT: &str = concat!("peryx-ha-distributed/", env!("CARGO_PKG_VERSION"));

/// Building an [`HttpBlobTransport`] failed before any request left the process.
#[derive(Debug, thiserror::Error)]
pub enum HttpBlobError {
    #[error("peer replication token must not be empty")]
    EmptyToken,
    #[error("invalid peer URL {0:?}")]
    InvalidBase(String),
}

/// A bearer-authenticated HTTP [`BlobTransport`] that fetches a blob, or a range of it, from a peer's
/// blob-serving endpoint.
#[derive(Clone)]
pub struct HttpBlobTransport {
    http: Client,
    blobs_url: Url,
    token: String,
    limits: TransferLimits,
}

impl fmt::Debug for HttpBlobTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpBlobTransport")
            .field("blobs_url", &self.blobs_url)
            .field("limits", &self.limits)
            .field("token", &"<redacted>")
            .finish_non_exhaustive()
    }
}

fn endpoint_url(base: &Url, path: &str) -> Url {
    let mut url = base.clone();
    url.set_path(&format!("{}{path}", base.path()));
    url
}

impl HttpBlobTransport {
    /// Build a transport rooted at the peer server URL, bounding each request with `timeout` and each
    /// blob with `limits`.
    ///
    /// # Errors
    /// Returns [`HttpBlobError`] for an empty token or a URL that is not a usable HTTP(S) base.
    ///
    /// # Panics
    /// Panics if the HTTP client cannot be built, which a static user agent and a duration timeout over
    /// the guaranteed `rustls` provider never provoke.
    pub fn new(
        base: &str,
        token: impl Into<String>,
        limits: TransferLimits,
        timeout: Duration,
    ) -> Result<Self, HttpBlobError> {
        let token = token.into();
        if token.is_empty() {
            return Err(HttpBlobError::EmptyToken);
        }
        let Ok(mut base_url) = Url::parse(base) else {
            return Err(HttpBlobError::InvalidBase(base.to_owned()));
        };
        if !matches!(base_url.scheme(), "http" | "https") || base_url.cannot_be_a_base() {
            return Err(HttpBlobError::InvalidBase(base.to_owned()));
        }
        if !base_url.path().ends_with('/') {
            base_url.set_path(&format!("{}/", base_url.path()));
        }
        base_url.set_query(None);
        base_url.set_fragment(None);
        let blobs_url = endpoint_url(&base_url, BLOBS_PATH);
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(timeout)
            .build()
            .expect("a reqwest client with a static user agent and a duration timeout always builds");
        Ok(Self {
            http,
            blobs_url,
            token,
            limits,
        })
    }
}

/// Map a transport-layer failure to its retryable [`TransportError`]: a deadline becomes a
/// [`Timeout`](TransportError::Timeout), any other connection loss a
/// [`Disconnected`](TransportError::Disconnected).
fn classify_loss(error: &reqwest::Error) -> TransportError {
    if error.is_timeout() {
        TransportError::Timeout
    } else {
        TransportError::Disconnected
    }
}

/// The inclusive `Range` header value covering `length` bytes from `offset`, as `bytes=first-last`.
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
            .get(&url)
            .bearer_auth(&self.token)
            .header(ACCEPT_ENCODING, "identity");
        if let Some(range) = request.range {
            builder = builder.header(RANGE, range_header(range));
        }
        let mut response = builder.send().await.map_err(|error| classify_loss(&error))?;
        let status = response.status();
        if status == StatusCode::UNAUTHORIZED {
            return Err(TransportError::Unauthenticated);
        }
        if status == StatusCode::NOT_FOUND {
            return Err(TransportError::BlobNotFound {
                digest: request.digest.as_str().to_owned(),
            });
        }
        // A transient 5xx (an overloaded or restarting peer) earns a backoff and retry, so it maps to
        // the retryable `ServerError`. `501 Not Implemented` is a permanent capability gap, so it stays
        // terminal alongside the 4xx client errors.
        if status.is_server_error() && status != StatusCode::NOT_IMPLEMENTED {
            return Err(TransportError::ServerError {
                status: status.as_u16(),
            });
        }
        if !status.is_success() {
            return Err(TransportError::BadStatus {
                status: status.as_u16(),
            });
        }
        let cap = self.limits.max_encoded_bytes.get();
        if let Some(length) = response.content_length()
            && length > cap
        {
            return Err(TransportError::FrameTooLarge {
                limit: cap,
                actual: length,
            });
        }
        let mut body: Vec<u8> = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| classify_loss(&error))? {
            let total = body.len() as u64 + chunk.len() as u64;
            if total > cap {
                return Err(TransportError::FrameTooLarge {
                    limit: cap,
                    actual: total,
                });
            }
            body.extend_from_slice(&chunk);
        }
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
