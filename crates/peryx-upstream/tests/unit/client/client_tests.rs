use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;

use futures_util::TryStreamExt as _;
use rstest::rstest;
use url::Url;
use wiremock::matchers::{header, header_regex, method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{guarded_client, mount_get, mount_head};
use crate::client::{
    Auth, BOUNDED_READ_TIMEOUT, RANGE_SUPPRESSION_CAPACITY, RANGE_SUPPRESSION_TTL, READ_IDLE_TIMEOUT, RangeSession,
    UpstreamClient, UpstreamError, redact_url,
};

#[tokio::test]
async fn test_fetch_bytes() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files/artifact.bin"))
        .and(header("accept-encoding", "identity"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"artifactbytes".to_vec()))
        .mount(&server)
        .await;
    let client = guarded_client(&server);

    let bytes = client
        .fetch_bytes(&format!("{}/files/artifact.bin", server.uri()))
        .await
        .unwrap();

    assert_eq!(&bytes[..], b"artifactbytes");
}

#[tokio::test]
async fn test_send_validated_uses_modification_time_without_etag() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/"))
        .respond_with(ResponseTemplate::new(304))
        .expect(1)
        .mount(&server)
        .await;
    let client = guarded_client(&server);

    let response = client
        .send_validated(
            client.base().clone(),
            "application/json",
            None,
            Some("Wed, 15 Jul 2026 12:00:00 GMT"),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), reqwest::StatusCode::NOT_MODIFIED);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests[0].headers.get("if-modified-since").unwrap().to_str().unwrap(),
        "Wed, 15 Jul 2026 12:00:00 GMT"
    );
}

#[tokio::test]
async fn test_send_validated_prefers_etag() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/"))
        .and(header("if-none-match", "catalog-etag"))
        .respond_with(ResponseTemplate::new(304))
        .expect(1)
        .mount(&server)
        .await;
    let client = guarded_client(&server);

    client
        .send_validated(
            client.base().clone(),
            "application/json",
            Some("catalog-etag"),
            Some("Wed, 15 Jul 2026 12:00:00 GMT"),
        )
        .await
        .unwrap();

    let requests = server.received_requests().await.unwrap();
    assert!(requests[0].headers.get("if-modified-since").is_none());
}

#[tokio::test]
async fn test_fetch_bytes_limited_accepts_body_at_limit() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files/artifact.bin"))
        .and(header("accept-encoding", "identity"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"artifactbytes".to_vec()))
        .mount(&server)
        .await;
    let client = guarded_client(&server);

    let bytes = client
        .fetch_bytes_limited(&format!("{}/files/artifact.bin", server.uri()), b"artifactbytes".len())
        .await
        .unwrap();

    assert_eq!(&bytes[..], b"artifactbytes");
}

#[tokio::test]
async fn test_fetch_bytes_limited_rejects_content_length_over_limit() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files/artifact.bin"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"artifactbytes".to_vec()))
        .mount(&server)
        .await;
    let client = guarded_client(&server);

    let err = client
        .fetch_bytes_limited(&format!("{}/files/artifact.bin", server.uri()), 9)
        .await
        .unwrap_err();

    assert_eq!(err.user_message(), "upstream response exceeds the 9-byte limit");
}

#[tokio::test]
async fn test_stream_bytes_streams_file() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files/artifact.bin"))
        .and(header("accept-encoding", "identity"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"artifactbytes".to_vec()))
        .mount(&server)
        .await;
    let client = guarded_client(&server);

    let bytes = client
        .stream_bytes(&format!("{}/files/artifact.bin", server.uri()))
        .await
        .unwrap()
        .try_fold(Vec::new(), |mut bytes, chunk| async move {
            bytes.extend_from_slice(&chunk);
            Ok(bytes)
        })
        .await
        .unwrap();

    assert_eq!(bytes, b"artifactbytes");
}

const PINNED_ETAG: &str = "\"generation-a\"";

async fn mount_pinned_head(server: &MockServer, request_path: &str, len: u64) {
    Mock::given(method("HEAD"))
        .and(path(request_path))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-length", len.to_string())
                .insert_header("etag", PINNED_ETAG),
        )
        .mount(server)
        .await;
}

fn partial(content_range: &str, body: &[u8]) -> ResponseTemplate {
    ResponseTemplate::new(206)
        .insert_header("content-range", content_range)
        .insert_header("etag", PINNED_ETAG)
        .set_body_bytes(body.to_vec())
}

async fn pinned_session(server: &MockServer, len: u64) -> RangeSession {
    mount_pinned_head(server, "/files/artifact.bin", len).await;
    guarded_client(server)
        .range_session(&format!("{}/files/artifact.bin", server.uri()))
        .await
        .unwrap()
}

fn served_session(server: &RangeServer) -> RangeSession {
    RangeSession::pinned(
        UpstreamClient::new(&server.origin()).unwrap(),
        Url::parse(&server.url()).unwrap(),
        5,
        PINNED_ETAG,
    )
}

#[tokio::test]
async fn test_range_session_pins_the_representation_from_head() {
    let server = MockServer::start().await;
    mount_pinned_head(&server, "/files/artifact.bin", 10).await;
    let client = guarded_client(&server);

    let session = client
        .range_session(&format!("{}/files/artifact.bin", server.uri()))
        .await
        .unwrap();

    assert_eq!((session.len(), session.is_empty()), (10, false));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_range_session_reports_an_empty_representation() {
    let server = MockServer::start().await;
    mount_pinned_head(&server, "/files/artifact.bin", 0).await;
    let client = guarded_client(&server);

    let session = client
        .range_session(&format!("{}/files/artifact.bin", server.uri()))
        .await
        .unwrap();

    assert!(session.is_empty());
}

#[tokio::test]
async fn test_range_session_reads_identity_bytes_under_the_pinned_validator() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files/artifact.bin"))
        .and(header("accept-encoding", "identity"))
        .and(header("range", "bytes=1-3"))
        .and(header("if-range", PINNED_ETAG))
        .respond_with(partial("bytes 1-3/5", b"hee"))
        .expect(1)
        .mount(&server)
        .await;
    let session = pinned_session(&server, 5).await;

    assert_eq!(&session.fetch_range(1, 3, 3).await.unwrap()[..], b"hee");
}

#[rstest]
#[case::unknown_total("bytes 1-3/*")]
#[case::another_length("bytes 1-3/9")]
#[tokio::test]
async fn test_range_session_rejects_a_response_length_other_than_the_pinned_one(#[case] content_range: &str) {
    let server = MockServer::start().await;
    mount_get(&server, "/files/artifact.bin", partial(content_range, b"hee")).await;
    let session = pinned_session(&server, 5).await;

    let err = session.fetch_range(1, 3, 3).await.unwrap_err();

    assert_eq!(
        err.to_string(),
        "upstream returned an invalid byte range response: range response reports a length other than the pinned 5"
    );
}

#[tokio::test]
async fn test_range_session_rejects_a_changed_entity_tag() {
    let server = MockServer::start().await;
    mount_get(
        &server,
        "/files/artifact.bin",
        ResponseTemplate::new(206)
            .insert_header("content-range", "bytes 1-3/5")
            .insert_header("etag", "\"generation-b\"")
            .set_body_bytes(b"hee".to_vec()),
    )
    .await;
    let session = pinned_session(&server, 5).await;

    let err = session.fetch_range(1, 3, 3).await.unwrap_err();

    assert_eq!(
        err.to_string(),
        "upstream returned an invalid byte range response: range response carries a different entity tag"
    );
}

#[tokio::test]
async fn test_range_session_rejects_a_response_that_left_the_pinned_url() {
    let server = MockServer::start().await;
    mount_get(
        &server,
        "/files/artifact.bin",
        ResponseTemplate::new(302).insert_header("location", "/files/other.bin"),
    )
    .await;
    mount_get(&server, "/files/other.bin", partial("bytes 1-3/5", b"hee")).await;
    let session = pinned_session(&server, 5).await;

    let err = session.fetch_range(1, 3, 3).await.unwrap_err();

    assert_eq!(
        err.to_string(),
        "upstream returned an invalid byte range response: range response left the pinned URL"
    );
}

#[tokio::test]
async fn test_range_session_rejects_a_range_past_the_representation() {
    let server = MockServer::start().await;
    let session = pinned_session(&server, 5).await;

    let err = session.fetch_range(3, 5, 3).await.unwrap_err();

    assert_eq!(
        err.to_string(),
        "upstream returned an invalid byte range response: range 3-5 leaves the 5-byte representation"
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_range_session_rejects_caller_budget_before_request() {
    let server = MockServer::start().await;
    let session = pinned_session(&server, 5).await;

    let err = session.fetch_range(0, 3, 3).await.unwrap_err();

    assert_eq!(
        err.to_string(),
        "upstream returned an invalid byte range response: requested range of 4 bytes exceeds the 3-byte memory limit"
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

/// A `200` answer to `If-Range` means the validator no longer matches, so the resource keeps its
/// ranged reads for the next generation instead of being suppressed.
#[tokio::test]
async fn test_range_session_rejects_a_whole_body_answer_without_suppressing_the_resource() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files/artifact.bin"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"whole".to_vec()))
        .expect(2)
        .mount(&server)
        .await;
    let session = pinned_session(&server, 5).await;

    for _ in 0..2 {
        assert!(matches!(
            session.fetch_range(1, 3, 3).await,
            Err(crate::RangeError::Unsupported)
        ));
    }
}

/// A resource can stop honoring `Range` while a session is open. The session skips the request
/// rather than pulling the whole body back for a few bytes.
#[tokio::test]
async fn test_pinned_range_read_stops_at_a_suppressed_resource() {
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path("/files/artifact.bin"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-length", "5")
                .insert_header("etag", PINNED_ETAG),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount_head(&server, "/files/artifact.bin", ResponseTemplate::new(405)).await;
    Mock::given(method("GET"))
        .and(path("/files/artifact.bin"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"whole".to_vec()))
        .expect(1)
        .mount(&server)
        .await;
    let client = guarded_client(&server);
    let url = format!("{}/files/artifact.bin", server.uri());
    let session = client.range_session(&url).await.unwrap();

    assert!(matches!(
        client.range_session(&url).await,
        Err(crate::RangeError::Unsupported)
    ));
    assert!(matches!(
        session.fetch_range(1, 3, 3).await,
        Err(crate::RangeError::Unsupported)
    ));
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
}

#[tokio::test]
async fn test_range_session_accepts_an_exact_chunked_body() {
    let server = RangeServer::start(RangeResponse::ExactChunked);

    let bytes = served_session(&server).fetch_range(1, 3, 3).await.unwrap();

    assert_eq!(bytes, b"hee".as_slice());
}

#[tokio::test]
async fn test_range_session_stops_at_first_excess_byte() {
    let server = RangeServer::start(RangeResponse::ExcessChunked);

    let err = served_session(&server).fetch_range(1, 3, 3).await.unwrap_err();

    assert_eq!(
        err.to_string(),
        "upstream returned an invalid byte range response: response body exceeds the expected 3 bytes"
    );
}

#[tokio::test]
async fn test_range_session_rejects_mismatched_content_length_before_body() {
    let server = RangeServer::start(RangeResponse::MismatchedContentLength);

    let err = served_session(&server).fetch_range(1, 3, 3).await.unwrap_err();

    assert_eq!(
        err.to_string(),
        "upstream returned an invalid byte range response: expected 3 bytes, received Content-Length 4"
    );
}

#[tokio::test]
async fn test_range_session_rejects_short_body() {
    let server = RangeServer::start(RangeResponse::ShortChunked);

    let err = served_session(&server).fetch_range(1, 3, 3).await.unwrap_err();

    assert_eq!(
        err.to_string(),
        "upstream returned an invalid byte range response: expected 3 bytes, received 2"
    );
}

#[test]
fn test_range_session_debug_redacts_the_pinned_url() {
    let session = RangeSession::pinned(
        UpstreamClient::new("https://upstream.example/api/").unwrap(),
        Url::parse("https://user:hunter2@upstream.example/files/artifact.bin?token=secret").unwrap(),
        5,
        PINNED_ETAG,
    );

    let debug = format!("{session:?}");

    assert!(debug.contains("https://upstream.example/files/artifact.bin"));
    assert!(!debug.contains("hunter2") && !debug.contains("secret"));
}

#[rstest]
#[case::missing_content_range(206, None, b"a".as_slice())]
#[case::non_bytes_unit(206, Some("items 0-0/5"), b"a".as_slice())]
#[case::missing_total(206, Some("bytes 0-0"), b"a".as_slice())]
#[case::missing_span(206, Some("bytes 0/5"), b"a".as_slice())]
#[case::span_mismatch(206, Some("bytes 1-1/5"), b"a".as_slice())]
#[case::non_numeric_total(206, Some("bytes 0-0/not-a-number"), b"a".as_slice())]
#[case::total_not_past_end(206, Some("bytes 0-0/0"), b"a".as_slice())]
#[case::unknown_total(206, Some("bytes 0-0/*"), b"a".as_slice())]
#[case::range_not_satisfiable(416, None, b"".as_slice())]
#[tokio::test]
async fn test_range_session_does_not_suppress_after_a_bad_probe_response(
    #[case] status: u16,
    #[case] content_range: Option<&str>,
    #[case] body: &[u8],
) {
    let server = MockServer::start().await;
    mount_head(&server, "/files/artifact.bin", ResponseTemplate::new(405)).await;
    let mut response = ResponseTemplate::new(status)
        .insert_header("etag", PINNED_ETAG)
        .set_body_bytes(body.to_vec());
    if let Some(content_range) = content_range {
        response = response.insert_header("content-range", content_range);
    }
    mount_get(&server, "/files/artifact.bin", response).await;
    let client = guarded_client(&server);

    for _ in 0..2 {
        client
            .range_session(&format!("{}/files/artifact.bin", server.uri()))
            .await
            .unwrap_err();
    }
    assert_eq!(server.received_requests().await.unwrap().len(), 4);
}

#[tokio::test]
async fn test_range_session_suppresses_only_the_resource_that_ignored_ranges() {
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .respond_with(ResponseTemplate::new(405))
        .expect(2)
        .mount(&server)
        .await;
    mount_get(
        &server,
        "/files/ignored.bin",
        ResponseTemplate::new(200).set_body_bytes(b"whole-file".to_vec()),
    )
    .await;
    mount_get(&server, "/files/supported.bin", partial("bytes 0-0/5", b"h")).await;
    let client = guarded_client(&server);
    let ignored = format!("{}/files/ignored.bin", server.uri());

    for _ in 0..2 {
        assert!(matches!(
            client.range_session(&ignored).await,
            Err(crate::RangeError::Unsupported)
        ));
    }

    assert_eq!(
        client
            .range_session(&format!("{}/files/supported.bin", server.uri()))
            .await
            .unwrap()
            .len(),
        5
    );
}

#[tokio::test]
async fn test_range_suppression_debug_omits_resource_urls() {
    let server = MockServer::start().await;
    mount_head(&server, "/files/ignored.bin", ResponseTemplate::new(405)).await;
    mount_get(&server, "/files/ignored.bin", ResponseTemplate::new(200)).await;
    let client = guarded_client(&server);

    client
        .range_session(&format!("{}/files/ignored.bin?token=secret", server.uri()))
        .await
        .unwrap_err();

    let debug = format!("{client:?}");
    assert!(debug.contains("RangeSuppressions { .. }"));
    assert!(!debug.contains("token=secret"));
}

#[tokio::test]
async fn test_range_suppression_expires() {
    let server = MockServer::start().await;
    mount_head(&server, "/files/ignored.bin", ResponseTemplate::new(405)).await;
    Mock::given(method("GET"))
        .and(path("/files/ignored.bin"))
        .respond_with(ResponseTemplate::new(200))
        .expect(2)
        .mount(&server)
        .await;
    let client = guarded_client(&server);
    let url = format!("{}/files/ignored.bin", server.uri());

    for _ in 0..2 {
        assert!(matches!(
            client.range_session(&url).await,
            Err(crate::RangeError::Unsupported)
        ));
    }
    tokio::time::pause();
    tokio::time::advance(RANGE_SUPPRESSION_TTL + std::time::Duration::from_nanos(1)).await;
    tokio::time::resume();
    assert!(matches!(
        client.range_session(&url).await,
        Err(crate::RangeError::Unsupported)
    ));
}

#[tokio::test]
async fn test_range_suppression_has_fixed_capacity() {
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path_regex(r"^/files/[0-9]+\.bin$"))
        .respond_with(ResponseTemplate::new(405))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/files/[0-9]+\.bin$"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let client = guarded_client(&server);

    for resource in 0..=RANGE_SUPPRESSION_CAPACITY {
        let url = format!("{}/files/{resource}.bin", server.uri());
        assert!(matches!(
            client.range_session(&url).await,
            Err(crate::RangeError::Unsupported)
        ));
    }
    assert!(matches!(
        client
            .range_session(&format!("{}/files/{RANGE_SUPPRESSION_CAPACITY}.bin", server.uri()))
            .await,
        Err(crate::RangeError::Unsupported)
    ));
    assert!(matches!(
        client.range_session(&format!("{}/files/0.bin", server.uri())).await,
        Err(crate::RangeError::Unsupported)
    ));

    assert_eq!(
        server.received_requests().await.unwrap().len(),
        2 * (RANGE_SUPPRESSION_CAPACITY + 1) + 2
    );
}

#[rstest]
#[case::missing_length(None, Some(PINNED_ETAG))]
#[case::missing_entity_tag(Some("10"), None)]
#[tokio::test]
async fn test_range_session_probes_when_head_omits_a_pin(
    #[case] content_length: Option<&str>,
    #[case] etag: Option<&str>,
) {
    let server = MockServer::start().await;
    let mut response = ResponseTemplate::new(200).insert_header("accept-ranges", "bytes");
    if let Some(content_length) = content_length {
        response = response.insert_header("content-length", content_length);
    }
    if let Some(etag) = etag {
        response = response.insert_header("etag", etag);
    }
    mount_head(&server, "/files/artifact.bin", response).await;
    Mock::given(method("GET"))
        .and(path("/files/artifact.bin"))
        .and(header("range", "bytes=0-0"))
        .respond_with(partial("bytes 0-0/10", b"a"))
        .expect(1)
        .mount(&server)
        .await;
    let client = guarded_client(&server);

    let session = client
        .range_session(&format!("{}/files/artifact.bin", server.uri()))
        .await
        .unwrap();

    assert_eq!(session.len(), 10);
}

#[rstest]
#[case::method_not_allowed(405)]
#[case::not_implemented(501)]
#[tokio::test]
async fn test_range_session_probes_when_head_is_unsupported(#[case] status: u16) {
    let server = MockServer::start().await;
    mount_head(&server, "/files/artifact.bin", ResponseTemplate::new(status)).await;
    Mock::given(method("GET"))
        .and(path("/files/artifact.bin"))
        .and(header("range", "bytes=0-0"))
        .respond_with(partial("bytes 0-0/10", b"a"))
        .expect(1)
        .mount(&server)
        .await;
    let client = guarded_client(&server);

    let session = client
        .range_session(&format!("{}/files/artifact.bin", server.uri()))
        .await
        .unwrap();

    assert_eq!(session.len(), 10);
}

#[rstest]
#[case::absent(None)]
#[case::weak(Some("W/\"generation-a\""))]
#[case::unquoted(Some("generation-a"))]
#[case::opening_quote_only(Some("\""))]
#[case::embedded_quote(Some("\"a\"b\""))]
#[tokio::test]
async fn test_range_session_needs_a_strong_entity_tag(#[case] etag: Option<&str>) {
    let server = MockServer::start().await;
    mount_head(&server, "/files/artifact.bin", ResponseTemplate::new(405)).await;
    let mut response = ResponseTemplate::new(206)
        .insert_header("content-range", "bytes 0-0/10")
        .set_body_bytes(b"a".to_vec());
    if let Some(etag) = etag {
        response = response.insert_header("etag", etag);
    }
    mount_get(&server, "/files/artifact.bin", response).await;
    let client = guarded_client(&server);

    assert!(matches!(
        client
            .range_session(&format!("{}/files/artifact.bin", server.uri()))
            .await,
        Err(crate::RangeError::Unsupported)
    ));
}

#[tokio::test]
async fn test_range_session_rejects_non_success_without_an_error_status() {
    let server = MockServer::start().await;
    mount_head(&server, "/files/artifact.bin", ResponseTemplate::new(304)).await;
    let client = guarded_client(&server);

    let err = client
        .range_session(&format!("{}/files/artifact.bin", server.uri()))
        .await
        .unwrap_err();

    assert_eq!(
        err.to_string(),
        "upstream returned an invalid byte range response: HEAD returned a non-success response"
    );
}

#[rstest]
#[case::unauthorized(401)]
#[case::not_found(404)]
#[tokio::test]
async fn test_head_failure_does_not_suppress_another_resource(#[case] status: u16) {
    let server = MockServer::start().await;
    mount_head(&server, "/files/rejected.bin", ResponseTemplate::new(status)).await;
    mount_pinned_head(&server, "/files/supported.bin", 10).await;
    let client = guarded_client(&server);

    client
        .range_session(&format!("{}/files/rejected.bin", server.uri()))
        .await
        .unwrap_err();

    assert_eq!(
        client
            .range_session(&format!("{}/files/supported.bin", server.uri()))
            .await
            .unwrap()
            .len(),
        10
    );
}

#[tokio::test]
async fn test_new_adds_trailing_slash() {
    let client = UpstreamClient::new("https://upstream.example/artifacts").unwrap();

    assert_eq!(client.base_url(), "https://upstream.example/artifacts/");
}

#[test]
fn test_new_rejects_invalid_url() {
    let err = UpstreamClient::new("not a url").unwrap_err();
    assert!(matches!(err, UpstreamError::Url(_)));
    assert_eq!(err.user_message(), "invalid upstream URL: relative URL without a base");
}

#[tokio::test]
async fn test_fetch_bytes_preserves_basic_auth_on_same_host_redirect() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/redirect/artifact.bin"))
        .and(header_regex("authorization", "^Basic "))
        .respond_with(
            ResponseTemplate::new(302).insert_header("location", format!("{}/files/artifact.bin", server.uri())),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/artifact.bin"))
        .and(header_regex("authorization", "^Basic "))
        .and(header("accept-encoding", "identity"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"artifactbytes".to_vec()))
        .mount(&server)
        .await;
    let client = UpstreamClient::with_auth(
        &format!("{}/api/", server.uri()),
        Auth::Basic {
            username: "client".to_owned(),
            password: "secret".to_owned(),
        },
    )
    .unwrap();

    let bytes = client
        .fetch_bytes(&format!("{}/redirect/artifact.bin", server.uri()))
        .await
        .unwrap();

    assert_eq!(&bytes[..], b"artifactbytes");
}

#[tokio::test]
async fn test_fetch_bytes_strips_basic_auth_on_cross_origin_redirect() {
    let origin = MockServer::start().await;
    let target = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/redirect/artifact.bin"))
        .and(header_regex("authorization", "^Basic "))
        .respond_with(ResponseTemplate::new(302).insert_header("location", format!("{}/artifact.bin", target.uri())))
        .mount(&origin)
        .await;
    Mock::given(method("GET"))
        .and(path("/artifact.bin"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"artifactbytes".to_vec()))
        .mount(&target)
        .await;
    let client = UpstreamClient::with_auth(
        &format!("{}/api/", origin.uri()),
        Auth::Basic {
            username: "reader".to_owned(),
            password: "secret".to_owned(),
        },
    )
    .unwrap();

    let bytes = client
        .fetch_bytes(&format!("{}/redirect/artifact.bin", origin.uri()))
        .await
        .unwrap();

    assert_eq!(&bytes[..], b"artifactbytes");
    assert_eq!(
        target.received_requests().await.unwrap()[0]
            .headers
            .get("authorization"),
        None
    );
}

#[tokio::test]
async fn test_fetch_bytes_does_not_authenticate_a_direct_cross_origin_url() {
    let origin = MockServer::start().await;
    let target = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/artifact.bin"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"artifactbytes".to_vec()))
        .mount(&target)
        .await;
    let client = UpstreamClient::with_auth(
        &format!("{}/api/", origin.uri()),
        Auth::Basic {
            username: "reader".to_owned(),
            password: "secret".to_owned(),
        },
    )
    .unwrap();

    let bytes = client
        .fetch_bytes(&format!("{}/artifact.bin", target.uri()))
        .await
        .unwrap();

    assert_eq!(&bytes[..], b"artifactbytes");
    assert_eq!(
        target.received_requests().await.unwrap()[0]
            .headers
            .get("authorization"),
        None
    );
}

#[test]
fn test_auth_status_redacts_basic_credentials_and_url_secrets() {
    let client = UpstreamClient::with_auth(
        "https://user:pass@example.invalid/api/?token=secret#frag",
        Auth::Basic {
            username: "client".to_owned(),
            password: "secret".to_owned(),
        },
    )
    .unwrap();

    assert_eq!(client.auth_status().as_str(), "basic");
    assert_eq!(client.redacted_base_url(), "https://example.invalid/api/");
}

#[rstest]
#[case::none(Auth::None, "none")]
#[case::basic(Auth::Basic { username: "reader".to_owned(), password: "secret".to_owned() }, "basic")]
#[case::bearer(Auth::Bearer("secret".to_owned()), "bearer")]
fn test_auth_status_classifies_the_configured_credential(#[case] auth: Auth, #[case] expected: &str) {
    let client = UpstreamClient::with_auth("https://example.invalid/api/", auth).unwrap();

    assert_eq!(client.auth_status().as_str(), expected);
}

#[test]
fn test_auth_returns_the_configured_credentials() {
    let auth = Auth::Basic {
        username: "alice".to_owned(),
        password: "s3cret".to_owned(),
    };
    let client = UpstreamClient::with_auth("https://example.invalid/api/", auth.clone()).unwrap();
    assert_eq!(client.auth().current().unwrap().auth(), &auth);
    assert_eq!(client.current_credential().unwrap().auth(), &auth);
    assert_eq!(
        UpstreamClient::new("https://example.invalid/api/")
            .unwrap()
            .current_credential()
            .unwrap()
            .auth(),
        &Auth::None
    );
}

#[tokio::test]
async fn test_send_validated_uses_the_cross_origin_client() {
    let origin = MockServer::start().await;
    let target = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&target)
        .await;
    let client = guarded_client(&origin);

    assert_eq!(
        client
            .send_validated(
                url::Url::parse(&format!("{}/api/", target.uri())).unwrap(),
                "application/json",
                None,
                None,
            )
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );
}

#[test]
fn test_redact_url_removes_credential_bearing_parts() {
    assert_eq!(
        redact_url("https://user:pass@example.invalid/api/?token=secret#frag"),
        "https://example.invalid/api/"
    );
    assert_eq!(redact_url("not a url"), "<invalid upstream URL>");
}

#[tokio::test]
async fn test_warm_reaches_the_upstream_host() {
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    let client = guarded_client(&server);
    assert_eq!(client.reachability().as_str(), "unknown");
    client.warm().await;
    assert_eq!(client.reachability().as_str(), "reachable");
}

#[tokio::test]
async fn test_warm_records_an_unreachable_upstream_host() {
    let client = UpstreamClient::new("http://127.0.0.1:0/api/").unwrap();
    client.warm().await;
    assert_eq!(client.reachability().as_str(), "unreachable");
}

#[derive(Clone, Copy)]
enum RangeResponse {
    ExactChunked,
    ExcessChunked,
    MismatchedContentLength,
    ShortChunked,
}

struct RangeServer {
    address: SocketAddr,
    release: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl RangeServer {
    fn start(response: RangeResponse) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (release, released) = channel();
        Self {
            address,
            release: Some(release),
            thread: Some(std::thread::spawn(move || serve_range(&listener, response, &released))),
        }
    }

    fn origin(&self) -> String {
        format!("http://{}", self.address)
    }

    fn url(&self) -> String {
        format!("{}/files/artifact.bin", self.origin())
    }
}

impl Drop for RangeServer {
    fn drop(&mut self) {
        drop(self.release.take());
        let _ = TcpStream::connect(self.address);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

fn serve_range(listener: &TcpListener, response: RangeResponse, released: &Receiver<()>) {
    let (mut socket, _) = listener.accept().unwrap();
    let mut request = [0; 1024];
    let _ = socket.read(&mut request);
    let (body, hold_open): (&[u8], bool) = match response {
        RangeResponse::ExactChunked => (
            b"HTTP/1.1 206 Partial Content\r\ncontent-range: bytes 1-3/5\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n1\r\nh\r\n2\r\nee\r\n0\r\n\r\n",
            false,
        ),
        RangeResponse::ExcessChunked => (
            b"HTTP/1.1 206 Partial Content\r\ncontent-range: bytes 1-3/5\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n3\r\nhee\r\n1\r\nx\r\n",
            true,
        ),
        RangeResponse::MismatchedContentLength => (
            b"HTTP/1.1 206 Partial Content\r\ncontent-range: bytes 1-3/5\r\ncontent-length: 4\r\nconnection: close\r\n\r\n",
            true,
        ),
        RangeResponse::ShortChunked => (
            b"HTTP/1.1 206 Partial Content\r\ncontent-range: bytes 1-3/5\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n2\r\nhe\r\n0\r\n\r\n",
            false,
        ),
    };
    socket.write_all(body).unwrap();
    if hold_open {
        released.recv().unwrap_err();
    }
}

/// A read budget longer than the idle bound cannot be spent: a connection that goes quiet is dropped at
/// [`READ_IDLE_TIMEOUT`] first, so the extra budget is unreachable and a read near its deadline fails for
/// a reason its own constant does not name. Raising one of the two without the other is the way that
/// happens, and these are two of five upstream constants that all read 30 seconds today.
#[test]
fn test_the_read_budget_fits_inside_the_idle_bound() {
    assert!(BOUNDED_READ_TIMEOUT <= READ_IDLE_TIMEOUT);
}
