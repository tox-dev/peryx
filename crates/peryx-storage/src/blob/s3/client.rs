//! S3 operations backed by the official AWS SDK.

use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use aws_config::BehaviorVersion;
use aws_sdk_s3::Client;
use aws_sdk_s3::config::{Builder, Config, Region, RequestChecksumCalculation, ResponseChecksumValidation};
use aws_sdk_s3::error::ProvideErrorMetadata as _;
use aws_sdk_s3::primitives::{ByteStream, Length};
use aws_sdk_s3::types::{ChecksumAlgorithm, CompletedMultipartUpload, CompletedPart};
use bytes::Bytes;
use futures_util::StreamExt as _;
use futures_util::stream::BoxStream;
use tokio::sync::OnceCell;

use super::config::S3Config;

/// An S3 request that did not succeed.
#[derive(Debug, thiserror::Error)]
pub enum S3Error {
    #[error("object not found")]
    NotFound,
    #[error("object already exists")]
    AlreadyExists,
    #[error("conditional write conflicted with another request")]
    Conflict,
    #[error("multipart upload no longer exists")]
    NoSuchUpload,
    #[error("s3 request failed: {0}")]
    Request(String),
    #[error("s3 returned an invalid {0}")]
    InvalidResponse(&'static str),
}

/// Object metadata returned by `HEAD`.
#[derive(Debug, Clone, Copy)]
pub struct S3Head {
    pub bytes: u64,
}

/// A streaming `GET` response.
pub struct S3Get {
    pub total_bytes: u64,
    pub body: BoxStream<'static, Result<Bytes, S3Error>>,
}

/// One completed multipart part.
#[derive(Debug, Clone)]
pub struct S3Part {
    pub number: i32,
    pub etag: String,
    pub checksum: Option<String>,
}

/// An S3 client bound to one bucket.
#[derive(Debug, Clone)]
pub struct S3Client {
    config: S3Config,
    client: Arc<OnceCell<Client>>,
}

impl S3Client {
    /// Build a client that defers initialization to the first request and uses the AWS default
    /// credential provider chain.
    #[must_use]
    pub fn new(config: S3Config) -> Self {
        Self {
            config,
            client: Arc::new(OnceCell::new()),
        }
    }

    #[must_use]
    pub const fn config(&self) -> &S3Config {
        &self.config
    }

    async fn client(&self) -> &Client {
        self.client
            .get_or_init(|| async {
                let shared = aws_config::defaults(BehaviorVersion::latest())
                    .region(Region::new(self.config.region.clone()))
                    .retry_config(
                        aws_config::retry::RetryConfig::standard()
                            .with_max_attempts(self.config.max_retries.saturating_add(1)),
                    )
                    .timeout_config(
                        aws_config::timeout::TimeoutConfig::builder()
                            .operation_timeout(self.config.request_timeout)
                            .operation_attempt_timeout(self.config.request_timeout)
                            .build(),
                    )
                    .load()
                    .await;
                Client::from_conf(Self::service_config(&self.config, Builder::from(&shared)))
            })
            .await
    }

    fn service_config(config: &S3Config, builder: Builder) -> Config {
        // With checksums off the SDK must not fall back to its default algorithm, or the endpoint
        // the operator disabled them for still receives a checksum on every write.
        let checksums = if config.checksum_writes {
            RequestChecksumCalculation::WhenSupported
        } else {
            RequestChecksumCalculation::WhenRequired
        };
        builder
            .endpoint_url(config.endpoint.as_str())
            .force_path_style(config.force_path_style())
            .request_checksum_calculation(checksums)
            .response_checksum_validation(ResponseChecksumValidation::WhenSupported)
            .build()
    }

    /// Confirm the bucket is reachable.
    ///
    /// # Errors
    /// Returns [`S3Error`] when the bucket cannot be read.
    pub async fn health(&self) -> Result<(), S3Error> {
        self.client()
            .await
            .get_bucket_location()
            .bucket(&self.config.bucket)
            .send()
            .await
            .map_err(aws_sdk_s3::Error::from)
            .map(drop)
            .map_err(|error| map_sdk_error(&error))
    }

    /// Return object metadata, or `None` when the object is absent.
    ///
    /// # Errors
    /// Returns [`S3Error`] when metadata cannot be read.
    pub async fn head(&self, key: &str) -> Result<Option<S3Head>, S3Error> {
        match self
            .client()
            .await
            .head_object()
            .bucket(&self.config.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(output) => Ok(Some(S3Head {
                bytes: object_length(output.content_length())?,
            })),
            Err(error) => match map_sdk_error(&error.into()) {
                S3Error::NotFound => Ok(None),
                error => Err(error),
            },
        }
    }

    /// Stream an object or end-exclusive byte range.
    ///
    /// # Errors
    /// Returns [`S3Error`] when the request or body stream fails.
    pub async fn get(&self, key: &str, range: Option<Range<u64>>) -> Result<S3Get, S3Error> {
        // An HTTP byte range is inclusive, so an empty end-exclusive slice has no representation and
        // reads no bytes; serve it directly instead of emitting a malformed `bytes=N-{N-1}` header.
        if range.as_ref().is_some_and(Range::is_empty) {
            return Ok(S3Get {
                total_bytes: 0,
                body: futures_util::stream::empty().boxed(),
            });
        }
        let deadline = tokio::time::Instant::now()
            .checked_add(self.config.request_timeout)
            .ok_or_else(|| S3Error::Request("request timeout exceeds the supported duration".to_owned()))?;
        let mut request = self.client().await.get_object().bucket(&self.config.bucket).key(key);
        if let Some(range) = &range {
            request = request.range(format!("bytes={}-{}", range.start, range.end.saturating_sub(1)));
        }
        let output = tokio::time::timeout_at(deadline, request.send())
            .await
            .map_err(|error| S3Error::Request(error.to_string()))?
            .map_err(aws_sdk_s3::Error::from)
            .map_err(|error| map_sdk_error(&error))?;
        let total_bytes = resolve_total_bytes(range.as_ref(), output.content_range(), output.content_length())?;
        let body = futures_util::stream::try_unfold((output.body, deadline), next_body_chunk).boxed();
        Ok(S3Get { total_bytes, body })
    }

    /// Create an object from a retryable file body when it does not already exist.
    ///
    /// # Errors
    /// Returns [`S3Error::AlreadyExists`] when the key exists, or [`S3Error`] for another failure.
    pub async fn put_file(&self, key: &str, path: &Path, checksum: &str) -> Result<(), S3Error> {
        let body = ByteStream::from_path(path)
            .await
            .map_err(|error| S3Error::Request(error.to_string()))?;
        let mut request = self
            .client()
            .await
            .put_object()
            .bucket(&self.config.bucket)
            .key(key)
            .body(body);
        // Some S3-compatible endpoints reject the checksum header, so the operator disables it per
        // instance; sending it anyway would fail every write against such an endpoint.
        if self.config.checksum_writes {
            request = request.checksum_sha256(checksum);
        }
        // Some S3-compatible endpoints reject the `*` precondition, so the operator disables it per
        // instance; sending it anyway would fail every write against such an endpoint.
        if self.config.conditional_writes {
            request = request.if_none_match("*");
        }
        request
            .send()
            .await
            .map_err(aws_sdk_s3::Error::from)
            .map(drop)
            .map_err(|error| map_sdk_error(&error))
    }

    /// Delete an object. Absence is successful.
    ///
    /// # Errors
    /// Returns [`S3Error`] when deletion fails.
    pub async fn delete(&self, key: &str) -> Result<(), S3Error> {
        match self
            .client()
            .await
            .delete_object()
            .bucket(&self.config.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(_) => Ok(()),
            Err(error) => match map_sdk_error(&error.into()) {
                S3Error::NotFound => Ok(()),
                error => Err(error),
            },
        }
    }

    /// Start a checksum-protected multipart upload.
    ///
    /// # Errors
    /// Returns [`S3Error`] when creation fails or no upload id is returned.
    pub async fn create_multipart(&self, key: &str) -> Result<String, S3Error> {
        let mut request = self
            .client()
            .await
            .create_multipart_upload()
            .bucket(&self.config.bucket)
            .key(key);
        if self.config.checksum_writes {
            request = request.checksum_algorithm(ChecksumAlgorithm::Sha256);
        }
        request
            .send()
            .await
            .map_err(aws_sdk_s3::Error::from)
            .map_err(|error| map_sdk_error(&error))?
            .upload_id
            .ok_or(S3Error::InvalidResponse("multipart upload id"))
    }

    /// Upload one checksum-protected part.
    ///
    /// # Errors
    /// Returns [`S3Error`] when the part cannot be uploaded or its response is incomplete.
    pub async fn upload_part(
        &self,
        key: &str,
        upload_id: &str,
        number: i32,
        path: &Path,
        offset: u64,
        bytes: u64,
    ) -> Result<S3Part, S3Error> {
        let body = ByteStream::read_from()
            .path(path)
            .offset(offset)
            .length(Length::Exact(bytes))
            .build()
            .await
            .map_err(|error| S3Error::Request(error.to_string()))?;
        let mut request = self
            .client()
            .await
            .upload_part()
            .bucket(&self.config.bucket)
            .key(key)
            .upload_id(upload_id)
            .part_number(number)
            .body(body);
        if self.config.checksum_writes {
            request = request.checksum_algorithm(ChecksumAlgorithm::Sha256);
        }
        let output = request
            .send()
            .await
            .map_err(aws_sdk_s3::Error::from)
            .map_err(|error| map_sdk_error(&error))?;
        Ok(S3Part {
            number,
            etag: output.e_tag.ok_or(S3Error::InvalidResponse("part ETag"))?,
            checksum: self
                .config
                .checksum_writes
                .then(|| output.checksum_sha256.ok_or(S3Error::InvalidResponse("part checksum")))
                .transpose()?,
        })
    }

    /// Complete a multipart upload only when the key is absent.
    ///
    /// # Errors
    /// Returns [`S3Error::AlreadyExists`] when the key exists, [`S3Error::Conflict`] when another
    /// conditional write races this one, or [`S3Error`] for another failure.
    pub async fn complete_multipart(&self, key: &str, upload_id: &str, parts: Vec<S3Part>) -> Result<(), S3Error> {
        let parts = parts
            .into_iter()
            .map(|part| {
                let mut completed = CompletedPart::builder().part_number(part.number).e_tag(part.etag);
                if let Some(checksum) = part.checksum {
                    completed = completed.checksum_sha256(checksum);
                }
                completed.build()
            })
            .collect();
        let mut request = self
            .client()
            .await
            .complete_multipart_upload()
            .bucket(&self.config.bucket)
            .key(key)
            .upload_id(upload_id)
            .multipart_upload(CompletedMultipartUpload::builder().set_parts(Some(parts)).build());
        // Some S3-compatible endpoints reject the `*` precondition, so the operator disables it per
        // instance; sending it anyway would fail every write against such an endpoint.
        if self.config.conditional_writes {
            request = request.if_none_match("*");
        }
        request
            .send()
            .await
            .map_err(aws_sdk_s3::Error::from)
            .map(drop)
            .map_err(|error| map_sdk_error(&error))
    }

    /// Abort a multipart upload. An absent upload is successful.
    ///
    /// # Errors
    /// Returns [`S3Error`] when the abort fails.
    pub async fn abort_multipart(&self, key: &str, upload_id: &str) -> Result<(), S3Error> {
        match self
            .client()
            .await
            .abort_multipart_upload()
            .bucket(&self.config.bucket)
            .key(key)
            .upload_id(upload_id)
            .send()
            .await
        {
            Ok(_) => Ok(()),
            Err(error) => match map_sdk_error(&error.into()) {
                S3Error::NoSuchUpload => Ok(()),
                error => Err(error),
            },
        }
    }
}

async fn next_body_chunk(
    (mut body, deadline): (ByteStream, tokio::time::Instant),
) -> Result<Option<(Bytes, (ByteStream, tokio::time::Instant))>, S3Error> {
    match tokio::time::timeout_at(deadline, body.try_next()).await {
        Ok(Ok(Some(bytes))) => Ok(Some((bytes, (body, deadline)))),
        Ok(Ok(None)) => Ok(None),
        Ok(Err(error)) => Err(S3Error::Request(error.to_string())),
        Err(error) => Err(S3Error::Request(error.to_string())),
    }
}

fn object_length(length: Option<i64>) -> Result<u64, S3Error> {
    length
        .and_then(|value| u64::try_from(value).ok())
        .ok_or(S3Error::InvalidResponse("content length"))
}

/// Resolve the object's total size, rejecting any range the backend did not honor. An S3-compatible
/// endpoint or proxy can ignore `Range` and answer `200 OK` with the whole object, or return `206`
/// with a shifted interval; trusting the advertised range while streaming a different body would
/// hand the caller more bytes than it requested.
fn resolve_total_bytes(
    range: Option<&Range<u64>>,
    content_range: Option<&str>,
    content_length: Option<i64>,
) -> Result<u64, S3Error> {
    let Some(range) = range else {
        // An unranged read has no recovery path for a partial body, so an unsolicited `Content-Range`
        // is a protocol violation rather than a value to trust.
        return match content_range {
            Some(_) => Err(S3Error::InvalidResponse("content range")),
            None => object_length(content_length),
        };
    };
    let (start, end, total) = content_range
        .and_then(parse_content_range)
        .ok_or(S3Error::InvalidResponse("content range"))?;
    if start != range.start || end != range.end.saturating_sub(1) {
        return Err(S3Error::InvalidResponse("content range"));
    }
    Ok(total)
}

/// Parse a `Content-Range: bytes START-END/TOTAL` header into its numeric parts. A `*` total or any
/// other deviation yields `None`, so the caller rejects the response instead of guessing.
fn parse_content_range(value: &str) -> Option<(u64, u64, u64)> {
    let (interval, total) = value.strip_prefix("bytes ")?.split_once('/')?;
    let (start, end) = interval.split_once('-')?;
    Some((start.parse().ok()?, end.parse().ok()?, total.parse().ok()?))
}

fn map_sdk_error(error: &aws_sdk_s3::Error) -> S3Error {
    match error.code() {
        Some("NoSuchBucket" | "NoSuchKey" | "NotFound") => S3Error::NotFound,
        Some("PreconditionFailed") => S3Error::AlreadyExists,
        Some("ConditionalRequestConflict") => S3Error::Conflict,
        Some("NoSuchUpload") => S3Error::NoSuchUpload,
        _ => S3Error::Request(error.to_string()),
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/blob/s3/client/tests.rs"]
mod tests;
