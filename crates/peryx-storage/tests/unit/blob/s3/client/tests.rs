use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use aws_sdk_s3::config::Credentials;
use aws_sdk_s3::primitives::SdkBody;
use aws_smithy_http_client::test_util::{CaptureRequestReceiver, NeverClient, capture_request, infallible_client_fn};
use bytes::Bytes;
use futures_util::StreamExt as _;
use http_body::Frame;
use http_body_util::StreamBody;
use rstest::rstest;
use tokio::sync::{Barrier, OnceCell, oneshot};
use url::Url;

use super::super::S3Backend;
use super::super::config::S3Settings;
use super::{BehaviorVersion, Builder, Client, Region, S3Client, S3Config, S3Error, S3Get, S3Part};
use crate::blob::{BlobBackend, BlobStore, Digest};

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

fn capturing_client(config: S3Config, response: Option<http::Response<SdkBody>>) -> (S3Client, CaptureRequestReceiver) {
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
    assert_eq!(S3Error::NoSuchBucket.to_string(), "bucket not found");
    assert_eq!(S3Error::AlreadyExists.to_string(), "object already exists");
    assert_eq!(
        S3Error::Conflict.to_string(),
        "conditional write conflicted with another request"
    );
    assert_eq!(S3Error::NoSuchUpload.to_string(), "multipart upload no longer exists");
    assert_eq!(S3Error::GenerationChanged.to_string(), "object changed during read");
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

fn get_client_with_timeout(response: http::Response<SdkBody>, request_timeout: Duration) -> S3Client {
    capturing_client(
        S3Config::new(S3Settings {
            request_timeout,
            ..base_settings()
        })
        .unwrap(),
        Some(response),
    )
    .0
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

fn streamed_response(bytes: &'static [u8], interval: Duration) -> http::Response<SdkBody> {
    let chunks = bytes
        .iter()
        .map(|byte| Bytes::copy_from_slice(&[*byte]))
        .collect::<VecDeque<_>>();
    let body = StreamBody::new(
        futures_util::stream::unfold(chunks, move |mut chunks| async move {
            let chunk = chunks.pop_front()?;
            tokio::time::sleep(interval).await;
            Some((Ok::<_, Infallible>(Frame::data(chunk)), chunks))
        })
        .fuse(),
    );
    http::Response::builder()
        .status(200)
        .header(http::header::CONTENT_LENGTH, bytes.len())
        .body(SdkBody::from_body_1_x(body))
        .unwrap()
}

fn yielding_range_response(body: &'static [u8], content_range: &'static str) -> http::Response<SdkBody> {
    let mut pending = true;
    let mut body = Some(Bytes::from_static(body));
    let body = StreamBody::new(futures_util::stream::poll_fn(move |context| {
        if pending {
            pending = false;
            context.waker().wake_by_ref();
            return Poll::Pending;
        }
        Poll::Ready(body.take().map(|body| Ok::<_, Infallible>(Frame::data(body))))
    }));
    http::Response::builder()
        .status(206)
        .header(http::header::CONTENT_RANGE, content_range)
        .body(SdkBody::from_body_1_x(body))
        .unwrap()
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
#[case::mixed_case_unit(1..5, "Bytes 1-4/7", b"acka", 7)]
#[case::uppercase_unit(1..5, "BYTES 1-4/7", b"acka", 7)]
#[tokio::test]
async fn test_get_accepts_a_partial_response_matching_the_request(
    #[case] range: std::ops::Range<u64>,
    #[case] content_range: &str,
    #[case] expected: &'static [u8],
    #[case] total: u64,
) {
    let client = get_client(get_response(206, Some(content_range), None, expected));

    let get = client.get("cache/sha256/digest", Some(range), None).await.unwrap();

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

    let error = client.get("cache/sha256/digest", Some(1..5), None).await.err().unwrap();

    assert!(matches!(error, S3Error::InvalidResponse("content range")));
}

#[tokio::test]
async fn test_get_reads_a_whole_object_without_a_range() {
    let client = get_client(get_response(200, None, Some(7), b"package"));

    let get = client.get("cache/sha256/digest", None, None).await.unwrap();

    assert_eq!(get.total_bytes, 7);
    assert_eq!(collect_body(get).await, b"package");
}

#[tokio::test(start_paused = true)]
async fn test_get_budget_starts_after_lazy_client_initialization() {
    let timeout = Duration::from_secs(5);
    let setup_delay = timeout + Duration::from_secs(1);
    let config = S3Config::new(S3Settings {
        request_timeout: timeout,
        ..base_settings()
    })
    .unwrap();
    let (sdk_client, http_client) = never_sdk_client(&config, None);
    let client = S3Client {
        config,
        client: Arc::new(OnceCell::new()),
    };
    let initialization = Arc::clone(&client.client);
    let initialization_gate = Arc::new(Barrier::new(2));
    let request_gate = Arc::clone(&initialization_gate);
    let initialize = tokio::spawn(async move {
        initialization
            .get_or_init(|| async move {
                request_gate.wait().await;
                request_gate.wait().await;
                sdk_client
            })
            .await;
    });
    initialization_gate.wait().await;
    let mut request = Box::pin(client.get("cache/sha256/digest", None, None));
    assert!(matches!(futures_util::poll!(&mut request), Poll::Pending));

    let started = tokio::time::Instant::now();
    tokio::time::advance(setup_delay).await;
    initialization_gate.wait().await;
    let error = request.await.err().expect("expected request timeout");
    initialize.await.unwrap();

    assert_eq!(
        (started.elapsed(), error.to_string(), http_client.num_calls()),
        (
            setup_delay + timeout,
            "s3 request failed: deadline has elapsed".to_owned(),
            1
        )
    );
}

#[tokio::test(start_paused = true)]
async fn test_get_normalizes_the_sdk_timeout_at_the_deadline() {
    let timeout = Duration::from_secs(5);
    let config = S3Config::new(S3Settings {
        request_timeout: timeout,
        ..base_settings()
    })
    .unwrap();
    let (sdk_client, http_client) = never_sdk_client(&config, Some(timeout));
    let client = S3Client {
        config,
        client: Arc::new(OnceCell::from(sdk_client)),
    };
    let started = tokio::time::Instant::now();

    let error = client
        .get("cache/sha256/digest", None, None)
        .await
        .err()
        .expect("expected request timeout");

    assert_eq!(
        (started.elapsed(), error.to_string(), http_client.num_calls()),
        (timeout, "s3 request failed: deadline has elapsed".to_owned(), 1)
    );
}

#[tokio::test(start_paused = true)]
async fn test_get_preserves_a_ready_service_error_at_the_deadline() {
    let timeout = Duration::from_secs(5);
    let (body_polled_tx, mut body_polled_rx) = oneshot::channel();
    let (release_response_tx, release_response_rx) = oneshot::channel();
    let body = StreamBody::new(futures_util::stream::once(async move {
        body_polled_tx.send(()).unwrap();
        release_response_rx.await.unwrap();
        Ok::<_, Infallible>(Frame::data(Bytes::from_static(
            b"<Error><Code>NoSuchKey</Code></Error>",
        )))
    }));
    let response = http::Response::builder()
        .status(404)
        .header(http::header::CONTENT_TYPE, "application/xml")
        .body(SdkBody::from_body_1_x(body))
        .unwrap();
    let client = get_client_with_timeout(response, timeout);
    let mut request = Box::pin(client.get("cache/sha256/digest", None, None));
    tokio::select! {
        polled = &mut body_polled_rx => polled.unwrap(),
        _ = &mut request => panic!("request completed before releasing the response body"),
    }
    release_response_tx.send(()).unwrap();
    tokio::time::advance(timeout).await;

    let error = request.await.err().expect("expected service error");
    assert!(matches!(error, S3Error::NotFound));
}

fn never_sdk_client(config: &S3Config, sdk_timeout: Option<Duration>) -> (Client, NeverClient) {
    let http_client = NeverClient::new();
    let mut builder = Builder::new()
        .behavior_version(BehaviorVersion::latest())
        .credentials_provider(Credentials::new("id", "secret", None, None, "test"))
        .region(Region::new("us-east-1"))
        .http_client(http_client.clone());
    if let Some(timeout) = sdk_timeout {
        builder = builder.timeout_config(
            aws_config::timeout::TimeoutConfig::builder()
                .operation_timeout(timeout)
                .operation_attempt_timeout(timeout)
                .build(),
        );
    }
    (
        Client::from_conf(S3Client::service_config(config, builder)),
        http_client,
    )
}

#[tokio::test]
async fn test_get_rejects_an_unsolicited_partial_for_an_unranged_read() {
    let client = get_client(get_response(206, Some("bytes 0-3/7"), None, b"pack"));

    let error = client.get("cache/sha256/digest", None, None).await.err().unwrap();

    assert!(matches!(error, S3Error::InvalidResponse("content range")));
}

#[tokio::test]
async fn test_get_rejects_an_unranged_read_without_a_content_length() {
    let client = get_client(get_response(200, None, None, b""));

    let error = client.get("cache/sha256/digest", None, None).await.err().unwrap();

    assert!(matches!(error, S3Error::InvalidResponse("content length")));
}

#[tokio::test]
async fn test_get_serves_an_empty_range_without_a_request() {
    let client = get_client(get_response(500, None, None, b""));

    let get = client.get("cache/sha256/digest", Some(3..3), None).await.unwrap();

    assert_eq!(get.total_bytes, 0);
    assert!(collect_body(get).await.is_empty());
}

#[derive(Clone, Copy)]
enum StreamingOperation {
    Read,
    Verify,
    Materialize,
}

#[rstest]
#[case::read(StreamingOperation::Read)]
#[case::verify(StreamingOperation::Verify)]
#[case::materialize(StreamingOperation::Materialize)]
#[tokio::test(start_paused = true)]
async fn test_progressing_downloads_renew_the_body_timeout(#[case] operation: StreamingOperation) {
    let timeout = Duration::from_secs(5);
    let staging = tempfile::tempdir().unwrap();
    let backend = S3Backend {
        client: get_client_with_timeout(streamed_response(b"package", Duration::from_secs(4)), timeout),
        staging: BlobStore::new(staging.path()),
        acquisitions: Arc::default(),
    };
    let digest = Digest::of(b"package");
    let started = tokio::time::Instant::now();

    match operation {
        StreamingOperation::Read => {
            assert_eq!(
                backend.open(digest, None).await.unwrap().collect(7).await.unwrap(),
                b"package"
            );
        }
        StreamingOperation::Verify => assert!(backend.verify(digest).await.unwrap()),
        StreamingOperation::Materialize => {
            assert_eq!(
                std::fs::read(backend.materialize(digest).await.unwrap().path()).unwrap(),
                b"package"
            );
        }
    }
    assert!(started.elapsed() > timeout);
}

#[tokio::test(start_paused = true)]
async fn test_range_body_timeout_starts_when_the_consumer_polls() {
    let timeout = Duration::from_secs(5);
    let client = get_client_with_timeout(yielding_range_response(b"acka", "bytes 1-4/7"), timeout);
    let get = client.get("cache/sha256/digest", Some(1..5), None).await.unwrap();
    tokio::time::advance(timeout + Duration::from_secs(1)).await;

    assert_eq!(collect_body(get).await, b"acka");
}

#[tokio::test(start_paused = true)]
async fn test_get_body_reports_its_idle_timeout() {
    let body = StreamBody::new(futures_util::stream::pending::<Result<Frame<Bytes>, Infallible>>());
    let response = http::Response::builder()
        .status(200)
        .header(http::header::CONTENT_LENGTH, 1)
        .body(SdkBody::from_body_1_x(body))
        .unwrap();
    let client = get_client_with_timeout(response, Duration::from_secs(5));

    let mut get = client.get("cache/sha256/digest", None, None).await.unwrap();
    let error = get.body.next().await.unwrap().unwrap_err();

    assert!(matches!(error, S3Error::Request(_)));
}

const STAGED: &[u8] = b"expected";
const RESIDENT: &[u8] = b"existing";

fn conflicting_backend(
    staging: &std::path::Path,
    respond: impl Fn(&http::Request<SdkBody>) -> http::Response<SdkBody> + Send + Sync + 'static,
) -> (S3Backend, Arc<std::sync::Mutex<Vec<String>>>) {
    let methods = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = Arc::clone(&methods);
    let config = S3Config::new(base_settings()).unwrap();
    let service = S3Client::service_config(
        &config,
        Builder::new()
            .behavior_version(BehaviorVersion::latest())
            .credentials_provider(Credentials::new("id", "secret", None, None, "test"))
            .region(Region::new("us-east-1"))
            .http_client(infallible_client_fn(move |request: http::Request<SdkBody>| {
                recorded.lock().unwrap().push(request.method().to_string());
                respond(&request)
            })),
    );
    let backend = S3Backend {
        client: S3Client {
            config,
            client: Arc::new(OnceCell::from(Client::from_conf(service))),
        },
        staging: BlobStore::new(staging),
        acquisitions: Arc::default(),
    };
    (backend, methods)
}

fn xml_error(status: u16, code: &str) -> http::Response<SdkBody> {
    http::Response::builder()
        .status(status)
        .body(SdkBody::from(format!("<Error><Code>{code}</Code></Error>")))
        .unwrap()
}

fn resident_head(length: usize) -> http::response::Builder {
    http::Response::builder()
        .status(200)
        .header("Content-Length", length.to_string())
        .header("ETag", "\"resident\"")
}

fn resident_body(bytes: &'static [u8]) -> http::Response<SdkBody> {
    http::Response::builder()
        .status(200)
        .header("Content-Length", bytes.len().to_string())
        .body(SdkBody::from(bytes))
        .unwrap()
}

async fn commit_staged(backend: &S3Backend) -> Result<crate::blob::PlacementReceipt, crate::blob::BlobError> {
    let mut write = backend.begin().await.unwrap();
    write.write_chunk(Bytes::from_static(STAGED)).await.unwrap();
    write.commit(&Digest::of(STAGED)).await
}

#[tokio::test]
async fn test_conflict_over_matching_resident_bytes_stays_idempotent() {
    let staging = tempfile::tempdir().unwrap();
    let (backend, _) = conflicting_backend(staging.path(), |request| match request.method().as_str() {
        "PUT" => xml_error(412, "PreconditionFailed"),
        "HEAD" => resident_head(STAGED.len()).body(SdkBody::empty()).unwrap(),
        _ => resident_body(STAGED),
    });

    let receipt = commit_staged(&backend).await.unwrap();

    assert_eq!(receipt.digest, Digest::of(STAGED));
    assert_eq!(receipt.size, STAGED.len() as u64);
}

#[tokio::test]
async fn test_conflict_over_other_resident_bytes_yields_no_receipt() {
    let staging = tempfile::tempdir().unwrap();
    let (backend, _) = conflicting_backend(staging.path(), |request| match request.method().as_str() {
        "PUT" => xml_error(412, "PreconditionFailed"),
        "HEAD" => resident_head(RESIDENT.len()).body(SdkBody::empty()).unwrap(),
        _ => resident_body(RESIDENT),
    });

    let error = commit_staged(&backend).await.unwrap_err();

    assert_eq!(
        error.mismatch(),
        Some((Digest::of(STAGED).as_str(), Digest::of(RESIDENT).as_str()))
    );
}

#[tokio::test]
async fn test_conflict_over_a_different_length_skips_the_download() {
    let staging = tempfile::tempdir().unwrap();
    // Only the head answers; a read the commit should never issue would fail its pinned precondition.
    let (backend, methods) = conflicting_backend(staging.path(), |request| match request.method().as_str() {
        "HEAD" => resident_head(3).body(SdkBody::empty()).unwrap(),
        _ => xml_error(412, "PreconditionFailed"),
    });

    let error = commit_staged(&backend).await.unwrap_err();

    assert_eq!(error.kind(), crate::blob::BlobErrorKind::DigestMismatch);
    assert!(
        error.to_string().ends_with("size mismatch: expected 8 bytes, got 3"),
        "{error}"
    );
    assert_eq!(*methods.lock().unwrap(), ["PUT", "HEAD"]);
}

#[tokio::test]
async fn test_a_full_object_checksum_proves_the_conflict_without_a_read() {
    let staging = tempfile::tempdir().unwrap();
    let checksum = super::super::digest_checksum(&Digest::of(STAGED));
    let (backend, methods) = conflicting_backend(staging.path(), move |request| match request.method().as_str() {
        "HEAD" => resident_head(STAGED.len())
            .header("x-amz-checksum-sha256", &checksum)
            .header("x-amz-checksum-type", "FULL_OBJECT")
            .body(SdkBody::empty())
            .unwrap(),
        _ => xml_error(412, "PreconditionFailed"),
    });

    assert_eq!(commit_staged(&backend).await.unwrap().digest, Digest::of(STAGED));
    assert_eq!(*methods.lock().unwrap(), ["PUT", "HEAD"]);
}

#[tokio::test]
async fn test_a_composite_checksum_does_not_prove_the_conflict() {
    let staging = tempfile::tempdir().unwrap();
    let checksum = super::super::digest_checksum(&Digest::of(STAGED));
    let (backend, _) = conflicting_backend(staging.path(), move |request| match request.method().as_str() {
        "PUT" => xml_error(412, "PreconditionFailed"),
        "HEAD" => resident_head(STAGED.len())
            .header("x-amz-checksum-sha256", &checksum)
            .header("x-amz-checksum-type", "COMPOSITE")
            .body(SdkBody::empty())
            .unwrap(),
        _ => resident_body(RESIDENT),
    });

    let error = commit_staged(&backend).await.unwrap_err();

    assert_eq!(
        error.mismatch(),
        Some((Digest::of(STAGED).as_str(), Digest::of(RESIDENT).as_str()))
    );
}

#[tokio::test]
async fn test_conflict_over_an_object_that_vanished_yields_no_receipt() {
    let staging = tempfile::tempdir().unwrap();
    let (backend, _) = conflicting_backend(staging.path(), |request| match request.method().as_str() {
        "PUT" => xml_error(412, "PreconditionFailed"),
        _ => xml_error(404, "NoSuchKey"),
    });

    let error = commit_staged(&backend).await.unwrap_err();

    assert_eq!(error.kind(), crate::blob::BlobErrorKind::NotFound);
}

#[tokio::test]
async fn test_conflict_verification_reports_an_unreadable_head() {
    let staging = tempfile::tempdir().unwrap();
    let (backend, _) = conflicting_backend(staging.path(), |request| match request.method().as_str() {
        "PUT" => xml_error(412, "PreconditionFailed"),
        _ => xml_error(500, "InternalError"),
    });

    let error = commit_staged(&backend).await.unwrap_err();

    assert_eq!(error.kind(), crate::blob::BlobErrorKind::Io);
}

#[tokio::test]
async fn test_conflict_verification_requires_an_etag_to_pin_the_object() {
    let staging = tempfile::tempdir().unwrap();
    let (backend, _) = conflicting_backend(staging.path(), |request| match request.method().as_str() {
        "HEAD" => http::Response::builder()
            .status(200)
            .header("Content-Length", STAGED.len().to_string())
            .body(SdkBody::empty())
            .unwrap(),
        _ => xml_error(412, "PreconditionFailed"),
    });

    let error = commit_staged(&backend).await.unwrap_err();

    assert_eq!(
        std::error::Error::source(&error).unwrap().to_string(),
        "s3 returned an invalid ETag"
    );
}

#[tokio::test]
async fn test_a_replacement_during_verification_yields_no_receipt() {
    let staging = tempfile::tempdir().unwrap();
    // The conditional write and the pinned read fail the same precondition: the key was taken, then the
    // generation the head measured was replaced under the verifying read.
    let (backend, _) = conflicting_backend(staging.path(), |request| match request.method().as_str() {
        "HEAD" => resident_head(STAGED.len()).body(SdkBody::empty()).unwrap(),
        _ => xml_error(412, "PreconditionFailed"),
    });

    let error = commit_staged(&backend).await.unwrap_err();

    assert_eq!(
        std::error::Error::source(&error).unwrap().to_string(),
        "object changed during read"
    );
}
