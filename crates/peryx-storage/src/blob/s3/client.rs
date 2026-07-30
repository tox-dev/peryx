//! S3 operations backed by the official AWS SDK.

use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use aws_config::BehaviorVersion;
use aws_sdk_s3::Client;
use aws_sdk_s3::config::{Region, RequestChecksumCalculation, ResponseChecksumValidation};
use aws_sdk_s3::error::ProvideErrorMetadata as _;
use aws_sdk_s3::primitives::ByteStream;
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
                            .operation_attempt_timeout(self.config.request_timeout)
                            .build(),
                    )
                    .load()
                    .await;
                let service = aws_sdk_s3::config::Builder::from(&shared)
                    .endpoint_url(self.config.endpoint.as_str())
                    .force_path_style(self.config.force_path_style())
                    .request_checksum_calculation(RequestChecksumCalculation::WhenSupported)
                    .response_checksum_validation(ResponseChecksumValidation::WhenSupported)
                    .build();
                Client::from_conf(service)
            })
            .await
    }

    /// Confirm the bucket is reachable.
    ///
    /// # Errors
    /// Returns [`S3Error`] when the bucket cannot be read.
    pub async fn health(&self) -> Result<(), S3Error> {
        self.client()
            .await
            .head_bucket()
            .bucket(&self.config.bucket)
            .send()
            .await
            .map(drop)
            .map_err(map_sdk_error)
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
            Err(error) => match map_sdk_error(error) {
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
        let mut request = self.client().await.get_object().bucket(&self.config.bucket).key(key);
        if let Some(range) = range {
            request = request.range(format!("bytes={}-{}", range.start, range.end.saturating_sub(1)));
        }
        let output = request.send().await.map_err(map_sdk_error)?;
        let total_bytes = output
            .content_range()
            .and_then(|value| value.rsplit('/').next())
            .and_then(|value| value.parse().ok())
            .map_or_else(|| object_length(output.content_length()), Ok)?;
        let timeout = self.config.request_timeout;
        let body = futures_util::stream::unfold(output.body, move |mut body| async move {
            match tokio::time::timeout(timeout, body.try_next()).await {
                Ok(Ok(Some(bytes))) => Some((Ok(bytes), body)),
                Ok(Ok(None)) => None,
                Ok(Err(error)) => Some((Err(S3Error::Request(error.to_string())), body)),
                Err(error) => Some((Err(S3Error::Request(error.to_string())), body)),
            }
        })
        .boxed();
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
            .map(drop)
            .map_err(map_sdk_error)
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
            Err(error) => match map_sdk_error(error) {
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
            .map_err(map_sdk_error)?
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
        body: Bytes,
        checksum: String,
    ) -> Result<S3Part, S3Error> {
        let output = self
            .client()
            .await
            .upload_part()
            .bucket(&self.config.bucket)
            .key(key)
            .upload_id(upload_id)
            .part_number(number)
            .body(ByteStream::from(body))
            .checksum_sha256(&checksum)
            .send()
            .await
            .map_err(map_sdk_error)?;
        Ok(S3Part {
            number,
            etag: output.e_tag.ok_or(S3Error::InvalidResponse("part ETag"))?,
            checksum,
        })
    }

    /// Complete a multipart upload only when the key is absent.
    ///
    /// # Errors
    /// Returns [`S3Error::AlreadyExists`] when the key exists, [`S3Error::Conflict`] when another
    /// conditional write races this one, or [`S3Error`] for another failure.
    pub async fn complete_multipart(
        &self,
        key: &str,
        upload_id: &str,
        parts: Vec<S3Part>,
        bytes: u64,
    ) -> Result<(), S3Error> {
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
            .mpu_object_size(i64::try_from(bytes).map_err(|_| S3Error::InvalidResponse("object size"))?)
            .if_none_match("*")
            .send()
            .await
            .map(drop)
            .map_err(map_sdk_error)
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
            Err(error) => match map_sdk_error(error) {
                S3Error::NoSuchUpload => Ok(()),
                error => Err(error),
            },
        }
    }
}

fn object_length(length: Option<i64>) -> Result<u64, S3Error> {
    length
        .and_then(|value| u64::try_from(value).ok())
        .ok_or(S3Error::InvalidResponse("content length"))
}

fn map_sdk_error(error: impl Into<aws_sdk_s3::Error>) -> S3Error {
    let error = error.into();
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
    use super::S3Error;

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
}
