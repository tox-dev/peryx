use std::ops::Range;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use aws_config::BehaviorVersion;
use aws_sdk_s3::Client;
use aws_sdk_s3::config::{Builder, Config, Region, RequestChecksumCalculation, ResponseChecksumValidation};
use aws_sdk_s3::error::{ProvideErrorMetadata as _, SdkError};
use aws_sdk_s3::primitives::{ByteStream, Length};
use aws_sdk_s3::types::{ChecksumAlgorithm, ChecksumMode, ChecksumType, CompletedMultipartUpload, CompletedPart};
use bytes::Bytes;
use futures_util::StreamExt as _;
use futures_util::stream::BoxStream;
use tokio::sync::OnceCell;

use super::super::range::strip_bytes_unit;
use super::config::S3Config;

#[derive(Debug, thiserror::Error)]
pub enum S3Error {
    #[error("object not found")]
    NotFound,
    #[error("bucket not found")]
    NoSuchBucket,
    #[error("object already exists")]
    AlreadyExists,
    #[error("conditional write conflicted with another request")]
    Conflict,
    #[error("multipart upload no longer exists")]
    NoSuchUpload,
    #[error("object changed during read")]
    GenerationChanged,
    #[error("s3 request failed: {0}")]
    Request(String),
    #[error("s3 returned an invalid {0}")]
    InvalidResponse(&'static str),
}

#[derive(Debug, Clone)]
pub struct S3Head {
    pub bytes: u64,
    pub etag: Option<String>,
    /// The base64 SHA-256 the store computed over the whole byte stream, which is the only checksum
    /// type that hashes what a peryx digest hashes. A multipart object carries a composite value
    /// built from part digests, so it is dropped here rather than mistaken for a content digest.
    pub whole_object_sha256: Option<String>,
}

pub struct S3Get {
    pub total_bytes: u64,
    pub body: BoxStream<'static, Result<Bytes, S3Error>>,
}

#[derive(Debug, Clone)]
pub struct S3Part {
    pub number: i32,
    pub etag: String,
    pub checksum: Option<String>,
}

#[derive(Debug, Clone)]
pub struct S3Client {
    config: S3Config,
    client: Arc<OnceCell<Client>>,
}

impl S3Client {
    /// Defers initialization and credential lookup until the first request.
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
        // Disabling checksums must also disable the SDK's default checksum algorithm.
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

    /// Returns `None` when the object is absent.
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
            .checksum_mode(ChecksumMode::Enabled)
            .send()
            .await
        {
            Ok(output) => Ok(Some(S3Head {
                bytes: object_length(output.content_length())?,
                etag: output.e_tag().map(str::to_owned),
                whole_object_sha256: output
                    .checksum_sha256()
                    .filter(|_| output.checksum_type() == Some(&ChecksumType::FullObject))
                    .map(str::to_owned),
            })),
            Err(error) => match map_sdk_error(&error.into()) {
                S3Error::NotFound => Ok(None),
                error => Err(error),
            },
        }
    }

    /// Streams an object or end-exclusive byte range.
    ///
    /// # Errors
    /// Returns [`S3Error`] when the request or body stream fails.
    pub async fn get(&self, key: &str, range: Option<Range<u64>>, if_match: Option<&str>) -> Result<S3Get, S3Error> {
        // HTTP has no inclusive representation for an empty end-exclusive range.
        if range.as_ref().is_some_and(Range::is_empty) {
            return Ok(S3Get {
                total_bytes: 0,
                body: futures_util::stream::empty().boxed(),
            });
        }
        let client = self.client().await;
        let deadline = tokio::time::Instant::now()
            .checked_add(self.config.request_timeout)
            .ok_or_else(|| S3Error::Request("request timeout exceeds the supported duration".to_owned()))?;
        let mut request = client.get_object().bucket(&self.config.bucket).key(key);
        if let Some(range) = &range {
            request = request.range(format!("bytes={}-{}", range.start, range.end.saturating_sub(1)));
        }
        if let Some(etag) = if_match {
            request = request.if_match(etag);
        }
        let deadline_error = || S3Error::Request("deadline has elapsed".to_owned());
        let output = tokio::time::timeout_at(deadline, request.send())
            .await
            .map_err(|_| deadline_error())?;
        let output = match output {
            Err(SdkError::TimeoutError(_)) => return Err(deadline_error()),
            output => output,
        }
        .map_err(aws_sdk_s3::Error::from)
        .map_err(|error| {
            if if_match.is_some() && error.code() == Some("PreconditionFailed") {
                S3Error::GenerationChanged
            } else {
                map_sdk_error(&error)
            }
        })?;
        let total_bytes = resolve_total_bytes(range.as_ref(), output.content_range(), output.content_length())?;
        let body =
            futures_util::stream::try_unfold((output.body, self.config.request_timeout), next_body_chunk).boxed();
        Ok(S3Get { total_bytes, body })
    }

    /// Creates the object only when its key is absent.
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
        // Some S3-compatible endpoints reject checksum headers.
        if self.config.checksum_writes {
            request = request.checksum_sha256(checksum);
        }
        // Some S3-compatible endpoints reject the `*` precondition.
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

    /// Treats an absent object as a successful delete.
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

    /// Completes the upload only when the key is absent.
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
        // Some S3-compatible endpoints reject the `*` precondition.
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

    /// Treats an absent multipart upload as a successful abort.
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
    (mut body, idle_timeout): (ByteStream, Duration),
) -> Result<Option<(Bytes, (ByteStream, Duration))>, S3Error> {
    match tokio::time::timeout(idle_timeout, body.try_next()).await {
        Ok(Ok(Some(bytes))) => Ok(Some((bytes, (body, idle_timeout)))),
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

/// Rejects responses that ignore or shift the requested range, which would expose unrequested bytes.
fn resolve_total_bytes(
    range: Option<&Range<u64>>,
    content_range: Option<&str>,
    content_length: Option<i64>,
) -> Result<u64, S3Error> {
    let Some(range) = range else {
        // An unsolicited `Content-Range` makes an unranged response ambiguous.
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

/// Rejects wildcard totals and deviations from `bytes START-END/TOTAL` instead of guessing.
fn parse_content_range(value: &str) -> Option<(u64, u64, u64)> {
    let (interval, total) = strip_bytes_unit(value, ' ')?.split_once('/')?;
    let (start, end) = interval.split_once('-')?;
    Some((start.parse().ok()?, end.parse().ok()?, total.parse().ok()?))
}

fn map_sdk_error(error: &aws_sdk_s3::Error) -> S3Error {
    match error.code() {
        Some("NoSuchBucket") => S3Error::NoSuchBucket,
        Some("NoSuchKey" | "NotFound") => S3Error::NotFound,
        Some("PreconditionFailed") => S3Error::AlreadyExists,
        Some("ConditionalRequestConflict") => S3Error::Conflict,
        Some("NoSuchUpload") => S3Error::NoSuchUpload,
        _ => S3Error::Request(error.to_string()),
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/blob/s3/client/tests.rs"]
mod tests;
