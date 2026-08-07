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
mod tests {
    use std::convert::Infallible;
    use std::sync::Arc;
    use std::time::Duration;

    use aws_sdk_s3::config::Credentials;
    use aws_sdk_s3::primitives::SdkBody;
    use aws_smithy_http_client::test_util::{CaptureRequestReceiver, capture_request};
    use bytes::Bytes;
    use futures_util::StreamExt as _;
    use http_body::Frame;
    use http_body_util::StreamBody;
    use rstest::rstest;
    use tokio::sync::OnceCell;
    use url::Url;

    use super::super::config::S3Settings;
    use super::{BehaviorVersion, Builder, Client, Region, S3Client, S3Config, S3Error, S3Get, S3Part};

    const CHECKSUM: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

    fn base_settings() -> S3Settings {
        S3Settings {
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
            conditional_writes: true,
            checksum_writes: true,
        }
    }

    fn capturing_client(
        config: S3Config,
        response: Option<http::Response<SdkBody>>,
    ) -> (S3Client, CaptureRequestReceiver) {
        let (http_client, request) = capture_request(response);
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
        (client, request)
    }

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
        let config = S3Config::new(base_settings()).unwrap();
        let (client, request) = capturing_client(config, None);
        let stage = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(stage.path(), b"package").unwrap();

        client
            .put_file("cache/sha256/digest", stage.path(), CHECKSUM)
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

    #[rstest]
    #[case::declared(true, Some("*"))]
    #[case::disabled(false, None)]
    #[tokio::test]
    async fn test_put_file_conditions_create_on_conditional_writes(
        #[case] conditional_writes: bool,
        #[case] precondition: Option<&str>,
    ) {
        let config = S3Config::new(S3Settings {
            conditional_writes,
            ..base_settings()
        })
        .unwrap();
        let (client, request) = capturing_client(config, None);
        let stage = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(stage.path(), b"package").unwrap();

        client
            .put_file("cache/sha256/digest", stage.path(), CHECKSUM)
            .await
            .unwrap();

        assert_eq!(request.expect_request().headers().get("if-none-match"), precondition);
    }

    #[rstest]
    #[case::declared(true, Some("*"))]
    #[case::disabled(false, None)]
    #[tokio::test]
    async fn test_complete_multipart_conditions_create_on_conditional_writes(
        #[case] conditional_writes: bool,
        #[case] precondition: Option<&str>,
    ) {
        let config = S3Config::new(S3Settings {
            conditional_writes,
            ..base_settings()
        })
        .unwrap();
        let response = http::Response::builder()
            .status(200)
            .body(SdkBody::from(
                "<CompleteMultipartUploadResult><ETag>etag</ETag></CompleteMultipartUploadResult>",
            ))
            .unwrap();
        let (client, request) = capturing_client(config, Some(response));
        let part = S3Part {
            number: 1,
            etag: "part".to_owned(),
            checksum: Some("checksum".to_owned()),
        };

        client
            .complete_multipart("cache/sha256/digest", "upload-1", vec![part])
            .await
            .unwrap();

        assert_eq!(request.expect_request().headers().get("if-none-match"), precondition);
    }

    #[rstest]
    #[case::declared(true, Some(CHECKSUM))]
    #[case::disabled(false, None)]
    #[tokio::test]
    async fn test_put_file_attaches_checksum_on_checksum_writes(
        #[case] checksum_writes: bool,
        #[case] checksum: Option<&str>,
    ) {
        let config = S3Config::new(S3Settings {
            checksum_writes,
            ..base_settings()
        })
        .unwrap();
        let (client, request) = capturing_client(config, None);
        let stage = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(stage.path(), b"package").unwrap();

        client
            .put_file("cache/sha256/digest", stage.path(), CHECKSUM)
            .await
            .unwrap();

        assert_eq!(
            request.expect_request().headers().get("x-amz-checksum-sha256"),
            checksum
        );
    }

    #[rstest]
    #[case::declared(true, Some("SHA256"))]
    #[case::disabled(false, None)]
    #[tokio::test]
    async fn test_create_multipart_sets_algorithm_on_checksum_writes(
        #[case] checksum_writes: bool,
        #[case] algorithm: Option<&str>,
    ) {
        let config = S3Config::new(S3Settings {
            checksum_writes,
            ..base_settings()
        })
        .unwrap();
        let response = http::Response::builder()
            .status(200)
            .body(SdkBody::from(
                "<InitiateMultipartUploadResult><UploadId>u</UploadId></InitiateMultipartUploadResult>",
            ))
            .unwrap();
        let (client, request) = capturing_client(config, Some(response));

        assert_eq!(client.create_multipart("cache/sha256/digest").await.unwrap(), "u");

        assert_eq!(
            request.expect_request().headers().get("x-amz-checksum-algorithm"),
            algorithm
        );
    }

    #[rstest]
    #[case::declared(true, Some("SHA256"), Some(CHECKSUM.to_owned()))]
    #[case::disabled(false, None, None)]
    #[tokio::test]
    async fn test_upload_part_sets_algorithm_on_checksum_writes(
        #[case] checksum_writes: bool,
        #[case] algorithm: Option<&str>,
        #[case] part_checksum: Option<String>,
    ) {
        let config = S3Config::new(S3Settings {
            checksum_writes,
            ..base_settings()
        })
        .unwrap();
        let mut response = http::Response::builder().status(200).header("ETag", "part-etag");
        if checksum_writes {
            response = response.header("x-amz-checksum-sha256", CHECKSUM);
        }
        let (client, request) = capturing_client(config, Some(response.body(SdkBody::empty()).unwrap()));
        let stage = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(stage.path(), b"package").unwrap();

        let part = client
            .upload_part("cache/sha256/digest", "upload-1", 1, stage.path(), 0, 7)
            .await
            .unwrap();

        assert_eq!(part.checksum, part_checksum);
        assert_eq!(
            request.expect_request().headers().get("x-amz-sdk-checksum-algorithm"),
            algorithm
        );
    }

    #[rstest]
    #[case::present(Some("checksum".to_owned()), true)]
    #[case::absent(None, false)]
    #[tokio::test]
    async fn test_complete_multipart_part_checksum_matches_upload(
        #[case] part_checksum: Option<String>,
        #[case] sends_checksum: bool,
    ) {
        let config = S3Config::new(base_settings()).unwrap();
        let response = http::Response::builder()
            .status(200)
            .body(SdkBody::from(
                "<CompleteMultipartUploadResult><ETag>etag</ETag></CompleteMultipartUploadResult>",
            ))
            .unwrap();
        let (client, request) = capturing_client(config, Some(response));
        let part = S3Part {
            number: 1,
            etag: "part".to_owned(),
            checksum: part_checksum,
        };

        client
            .complete_multipart("cache/sha256/digest", "upload-1", vec![part])
            .await
            .unwrap();

        let captured = request.expect_request();
        let body = std::str::from_utf8(captured.body().bytes().unwrap()).unwrap();
        assert_eq!(body.contains("<ChecksumSHA256>"), sends_checksum);
    }

    fn get_client(response: http::Response<SdkBody>) -> S3Client {
        capturing_client(S3Config::new(base_settings()).unwrap(), Some(response)).0
    }

    fn get_response(
        status: u16,
        content_range: Option<&str>,
        content_length: Option<usize>,
        body: &'static [u8],
    ) -> http::Response<SdkBody> {
        let mut builder = http::Response::builder().status(status);
        if let Some(value) = content_range {
            builder = builder.header("Content-Range", value);
        }
        if let Some(length) = content_length {
            builder = builder.header("Content-Length", length.to_string());
        }
        builder.body(SdkBody::from(body.to_vec())).unwrap()
    }

    async fn collect_body(get: S3Get) -> Vec<u8> {
        let mut body = get.body;
        let mut bytes = Vec::new();
        while let Some(chunk) = body.next().await {
            bytes.extend_from_slice(&chunk.unwrap());
        }
        bytes
    }

    #[rstest]
    #[case::single_byte(5..6, "bytes 5-5/10", b"x", 10)]
    #[case::multi_byte(1..5, "bytes 1-4/7", b"acka", 7)]
    #[tokio::test]
    async fn test_get_accepts_a_partial_response_matching_the_request(
        #[case] range: std::ops::Range<u64>,
        #[case] content_range: &str,
        #[case] expected: &'static [u8],
        #[case] total: u64,
    ) {
        let client = get_client(get_response(206, Some(content_range), None, expected));

        let get = client.get("cache/sha256/digest", Some(range)).await.unwrap();

        assert_eq!(get.total_bytes, total);
        assert_eq!(collect_body(get).await, expected);
    }

    #[rstest]
    #[case::ignored_range(200, None, Some(7), b"package")]
    #[case::shifted_range(206, Some("bytes 2-5/7"), None, b"ckag")]
    #[case::truncated_range(206, Some("bytes 1-3/7"), None, b"ack")]
    #[case::missing_bytes_unit(206, Some("1-4/7"), None, b"acka")]
    #[case::missing_total(206, Some("bytes 1-4"), None, b"acka")]
    #[case::missing_interval(206, Some("bytes 14/7"), None, b"acka")]
    #[case::wildcard_total(206, Some("bytes 1-4/*"), None, b"acka")]
    #[case::malformed_start(206, Some("bytes a-4/7"), None, b"acka")]
    #[case::malformed_end(206, Some("bytes 1-b/7"), None, b"acka")]
    #[case::malformed_total(206, Some("bytes 1-4/x"), None, b"acka")]
    #[tokio::test]
    async fn test_get_rejects_a_range_the_backend_did_not_honor(
        #[case] status: u16,
        #[case] content_range: Option<&str>,
        #[case] content_length: Option<usize>,
        #[case] body: &'static [u8],
    ) {
        let client = get_client(get_response(status, content_range, content_length, body));

        let error = client.get("cache/sha256/digest", Some(1..5)).await.err().unwrap();

        assert!(matches!(error, S3Error::InvalidResponse("content range")));
    }

    #[tokio::test]
    async fn test_get_reads_a_whole_object_without_a_range() {
        let client = get_client(get_response(200, None, Some(7), b"package"));

        let get = client.get("cache/sha256/digest", None).await.unwrap();

        assert_eq!(get.total_bytes, 7);
        assert_eq!(collect_body(get).await, b"package");
    }

    #[tokio::test]
    async fn test_get_rejects_an_unsolicited_partial_for_an_unranged_read() {
        let client = get_client(get_response(206, Some("bytes 0-3/7"), None, b"pack"));

        let error = client.get("cache/sha256/digest", None).await.err().unwrap();

        assert!(matches!(error, S3Error::InvalidResponse("content range")));
    }

    #[tokio::test]
    async fn test_get_rejects_an_unranged_read_without_a_content_length() {
        let client = get_client(get_response(200, None, None, b""));

        let error = client.get("cache/sha256/digest", None).await.err().unwrap();

        assert!(matches!(error, S3Error::InvalidResponse("content length")));
    }

    #[tokio::test]
    async fn test_get_serves_an_empty_range_without_a_request() {
        let client = get_client(get_response(500, None, None, b""));

        let get = client.get("cache/sha256/digest", Some(3..3)).await.unwrap();

        assert_eq!(get.total_bytes, 0);
        assert!(collect_body(get).await.is_empty());
    }

    #[tokio::test]
    async fn test_get_body_reports_its_request_deadline() {
        let body = StreamBody::new(futures_util::stream::pending::<Result<Frame<Bytes>, Infallible>>());
        let response = http::Response::builder()
            .status(200)
            .header(http::header::CONTENT_LENGTH, 1)
            .body(SdkBody::from_body_1_x(body))
            .unwrap();
        let mut settings = base_settings();
        settings.request_timeout = Duration::from_millis(20);
        let (client, _) = capturing_client(S3Config::new(settings).unwrap(), Some(response));

        let mut get = client.get("cache/sha256/digest", None).await.unwrap();
        let error = get.body.next().await.unwrap().unwrap_err();

        assert!(matches!(error, S3Error::Request(_)));
    }
}
