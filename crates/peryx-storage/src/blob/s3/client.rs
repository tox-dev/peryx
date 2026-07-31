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
    pub checksum: String,
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
        builder
            .endpoint_url(config.endpoint.as_str())
            .force_path_style(config.force_path_style())
            .request_checksum_calculation(RequestChecksumCalculation::WhenSupported)
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
        let deadline = tokio::time::Instant::now()
            .checked_add(self.config.request_timeout)
            .ok_or_else(|| S3Error::Request("request timeout exceeds the supported duration".to_owned()))?;
        let mut request = self.client().await.get_object().bucket(&self.config.bucket).key(key);
        if let Some(range) = range {
            request = request.range(format!("bytes={}-{}", range.start, range.end.saturating_sub(1)));
        }
        let output = tokio::time::timeout_at(deadline, request.send())
            .await
            .map_err(|error| S3Error::Request(error.to_string()))?
            .map_err(aws_sdk_s3::Error::from)
            .map_err(|error| map_sdk_error(&error))?;
        let total_bytes = output
            .content_range()
            .and_then(|value| value.rsplit('/').next())
            .and_then(|value| value.parse().ok())
            .map_or_else(|| object_length(output.content_length()), Ok)?;
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
        self.client()
            .await
            .put_object()
            .bucket(&self.config.bucket)
            .key(key)
            .body(body)
            .checksum_sha256(checksum)
            .if_none_match("*")
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
        self.client()
            .await
            .create_multipart_upload()
            .bucket(&self.config.bucket)
            .key(key)
            .checksum_algorithm(ChecksumAlgorithm::Sha256)
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
        let output = self
            .client()
            .await
            .upload_part()
            .bucket(&self.config.bucket)
            .key(key)
            .upload_id(upload_id)
            .part_number(number)
            .body(body)
            .checksum_algorithm(ChecksumAlgorithm::Sha256)
            .send()
            .await
            .map_err(aws_sdk_s3::Error::from)
            .map_err(|error| map_sdk_error(&error))?;
        Ok(S3Part {
            number,
            etag: output.e_tag.ok_or(S3Error::InvalidResponse("part ETag"))?,
            checksum: output
                .checksum_sha256
                .ok_or(S3Error::InvalidResponse("part checksum"))?,
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
                CompletedPart::builder()
                    .part_number(part.number)
                    .e_tag(part.etag)
                    .checksum_sha256(part.checksum)
                    .build()
            })
            .collect();
        self.client()
            .await
            .complete_multipart_upload()
            .bucket(&self.config.bucket)
            .key(key)
            .upload_id(upload_id)
            .multipart_upload(CompletedMultipartUpload::builder().set_parts(Some(parts)).build())
            .if_none_match("*")
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
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use aws_sdk_s3::config::Credentials;
    use aws_smithy_http_client::test_util::capture_request;
    use tokio::sync::OnceCell;
    use url::Url;

    use super::super::config::S3Settings;
    use super::{BehaviorVersion, Builder, Client, Region, S3Client, S3Config, S3Error};

    #[test]
    fn test_error_messages_cover_every_variant() {
        assert_eq!(S3Error::NotFound.to_string(), "object not found");
        assert_eq!(S3Error::AlreadyExists.to_string(), "object already exists");
        assert_eq!(
            S3Error::Conflict.to_string(),
            "conditional write conflicted with another request"
        );
        assert_eq!(S3Error::NoSuchUpload.to_string(), "multipart upload no longer exists");
        assert_eq!(
            S3Error::Request("reset".to_owned()).to_string(),
            "s3 request failed: reset"
        );
        assert_eq!(
            S3Error::InvalidResponse("content length").to_string(),
            "s3 returned an invalid content length"
        );
    }

    #[tokio::test]
    async fn test_virtual_host_addressing_preserves_the_endpoint_base_path() {
        let config = S3Config::new(S3Settings {
            endpoint: "http://localhost:1/api".to_owned(),
            bucket: "peryx-tests".to_owned(),
            prefix: "cache".to_owned(),
            region: "us-east-1".to_owned(),
            path_style: false,
            request_timeout: Duration::from_secs(5),
            max_retries: 0,
            multipart_threshold: 16 << 20,
            part_size: 8 << 20,
            upload_concurrency: 1,
        })
        .unwrap();
        let (http_client, request) = capture_request(None);
        let service = S3Client::service_config(
            &config,
            Builder::new()
                .behavior_version(BehaviorVersion::latest())
                .credentials_provider(Credentials::new("id", "secret", None, None, "test"))
                .region(Region::new("us-east-1"))
                .http_client(http_client),
        );
        let client = S3Client {
            config,
            client: Arc::new(OnceCell::from(Client::from_conf(service))),
        };
        let stage = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(stage.path(), b"package").unwrap();

        client
            .put_file(
                "cache/sha256/digest",
                stage.path(),
                "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            )
            .await
            .unwrap();

        let request = request.expect_request();
        assert_eq!(request.method(), "PUT");
        let uri = Url::parse(request.uri()).unwrap();
        assert_eq!(
            uri.as_str(),
            "http://peryx-tests.localhost:1/api/cache/sha256/digest?x-id=PutObject"
        );
        assert_eq!(uri.host_str(), Some("peryx-tests.localhost"));
        assert_eq!(uri.port(), Some(1));
    }
}
